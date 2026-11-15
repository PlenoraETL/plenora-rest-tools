//! Own streaming bodies and the staged download lifecycle through publication.

use super::{
    ACCEPT_ENCODING, AsyncWriteExt, AttemptOutcome, Body, CONTENT_LENGTH, CONTENT_RANGE,
    DOWNLOAD_SEQUENCE, Digest, DownloadData, DownloadTarget, ETAG, EngineError, ErrorDetail,
    ErrorPhase, Form, HttpMethod, IF_RANGE, OpenOptions, Ordering, Part, Path, PathBuf,
    PendingResponse, PreparedFile, PreparedFileSource, PreparedRequest, PreparedStream, RANGE,
    ReaderStream, RetryDecision, RetrySession, Sha256, StreamExt, Transport, enter_phase, fs,
    is_retryable_transport_error, map_reqwest_error, response_headers, response_retry_after,
    set_request_header, sleep,
};
use crate::error::file_io;

struct DownloadState {
    temporary: PathBuf,
    file: Option<fs::File>,
    bytes_written: u64,
    bytes_received: u64,
    digest: Sha256,
    etag: Option<String>,
    expected_total: Option<u64>,
    /// Cleared once the staging file has been persisted or explicitly discarded.
    /// While set, dropping the state removes the partial file, which is what makes
    /// a cancelled or timed out download cancellation-safe.
    cleanup: bool,
}

impl Drop for DownloadState {
    fn drop(&mut self) {
        if !self.cleanup {
            return;
        }
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.temporary);
    }
}

impl Transport {
    pub async fn download(
        &self,
        mut request: PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
    ) -> Result<DownloadData, EngineError> {
        self.validate_request(&request)?;
        enter_phase(ErrorPhase::Connect);
        let _jar_lease = self.admit_cookie_jar(&mut request).await?;
        if target.resume {
            if request.method != HttpMethod::Get {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "resumable downloads require the GET method",
                )));
            }
            if request
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case(RANGE.as_str()))
                || request
                    .headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(IF_RANGE.as_str()))
            {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "managed resume cannot be combined with Range or If-Range headers",
                )));
            }
            set_request_header(
                &mut request.headers,
                ACCEPT_ENCODING.as_str(),
                "identity".to_owned(),
            );
        }
        let auth_stats = self.resolve_auth(&mut request).await?;
        enter_phase(ErrorPhase::Connect);
        let mut response = self
            .download_with_circuit(&request, target, success_statuses)
            .await?;
        response.auth_requests = auth_stats.network_requests;
        response.auth_retries = auth_stats.retries;
        response.network_requests = response
            .network_requests
            .saturating_add(auth_stats.network_requests);
        response.rate_limit_wait_ms = response
            .rate_limit_wait_ms
            .saturating_add(auth_stats.rate_limit_wait_ms);
        Ok(response)
    }
}

impl Transport {
    async fn download_with_circuit(
        &self,
        request: &PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
    ) -> Result<DownloadData, EngineError> {
        let admission = self.admit_circuit(request).await?;
        let result = self
            .download_authenticated(request, target, success_statuses)
            .await;
        if let Some(admission) = admission {
            let failed = match &result {
                Ok(response) => request
                    .circuit_breaker
                    .failure_statuses
                    .contains(&response.status),
                Err(EngineError::HttpStatus { status, .. }) => {
                    request.circuit_breaker.failure_statuses.contains(status)
                }
                Err(error) => is_retryable_transport_error(error),
            };
            self.record_circuit(&admission, &request.circuit_breaker, failed)
                .await;
        }
        result
    }

    async fn download_authenticated(
        &self,
        request: &PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
    ) -> Result<DownloadData, EngineError> {
        if !target.overwrite && fs::try_exists(&target.path).await.map_err(file_io)? {
            return Err(EngineError::FileIo(ErrorDetail::from(
                "destination already exists",
            )));
        }
        let mut state = create_download_state(&target.path).await?;
        let result = self
            .download_authenticated_with_state(request, target, success_statuses, &mut state)
            .await;
        if result.is_err() {
            discard_download_state(&mut state).await;
        }
        result
    }

    async fn download_authenticated_with_state(
        &self,
        request: &PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
        state: &mut DownloadState,
    ) -> Result<DownloadData, EngineError> {
        let mut retries = RetrySession::new(&request.retry, &request.method)?;
        let mut network_requests = 0_u64;
        let mut rate_limit_wait_ms = 0_u64;

        for attempt in retries.attempts() {
            let mut attempt_request = request.clone();
            let resume_etag = if target.resume && state.bytes_written > 0 {
                state.etag.clone()
            } else {
                None
            };
            let resumed = resume_etag.is_some();
            if let Some(etag) = resume_etag {
                set_request_header(
                    &mut attempt_request.headers,
                    RANGE.as_str(),
                    format!("bytes={}-", state.bytes_written),
                );
                set_request_header(&mut attempt_request.headers, IF_RANGE.as_str(), etag);
            }
            let pending = match self.send_once_response(&attempt_request, attempt > 1).await {
                Ok(pending) => pending,
                Err(error) => {
                    let RetryDecision::After(delay) =
                        retries.decide(attempt, AttemptOutcome::Failed(&error))
                    else {
                        return Err(error);
                    };
                    sleep(delay).await;
                    continue;
                }
            };
            network_requests = network_requests.saturating_add(pending.network_requests);
            rate_limit_wait_ms = rate_limit_wait_ms.saturating_add(pending.rate_limit_wait_ms);
            let status = pending.response.status().as_u16();
            let retry_after_ms = response_retry_after(&pending.response);
            if let RetryDecision::After(delay) = retries.decide(
                attempt,
                AttemptOutcome::Response {
                    status,
                    retry_after_ms,
                },
            ) {
                sleep(delay).await;
                continue;
            }

            match self
                .write_download_attempt(pending, target, success_statuses, state, resumed)
                .await
            {
                Ok(mut response) => {
                    response.attempts = attempt;
                    response.network_requests = network_requests;
                    response.rate_limit_wait_ms = rate_limit_wait_ms;
                    return Ok(response);
                }
                Err(error) => {
                    let RetryDecision::After(delay) =
                        retries.decide(attempt, AttemptOutcome::Failed(&error))
                    else {
                        return Err(error);
                    };
                    if !(target.resume
                        && state.bytes_written > 0
                        && state.etag.as_deref().is_some())
                    {
                        reset_download_state(state).await?;
                    }
                    sleep(delay).await;
                }
            }
        }

        Err(EngineError::Runtime(ErrorDetail::from(
            "download retry loop terminated unexpectedly",
        )))
    }
}

impl Transport {
    async fn write_download_attempt(
        &self,
        pending: PendingResponse,
        target: &DownloadTarget,
        success_statuses: &[u16],
        state: &mut DownloadState,
        resumed: bool,
    ) -> Result<DownloadData, EngineError> {
        let PendingResponse {
            response,
            final_url,
            network_requests,
            rate_limit_wait_ms,
            permit: _permit,
        } = pending;
        let status = response.status().as_u16();
        let headers = response_headers(&response);
        if !(200..300).contains(&status) && !success_statuses.contains(&status) {
            return Err(EngineError::HttpStatus { status });
        }
        if resumed && !matches!(status, 200 | 206) {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "resumed download returned a status other than 200 or 206",
            )));
        }
        let content_length = response_content_length(&response);
        let response_etag = strong_response_etag(&response);
        let expected_total = if status == 206 {
            let content_range = satisfied_content_range(&response)?;
            let total = content_range.total;
            let segment_length = content_range
                .end
                .checked_sub(content_range.start)
                .and_then(|length| length.checked_add(1))
                .ok_or_else(|| {
                    EngineError::InvalidResponse(ErrorDetail::from(
                        "download Content-Range has invalid bounds",
                    ))
                })?;
            if content_length.is_some_and(|length| length != segment_length) {
                return Err(EngineError::InvalidResponse(ErrorDetail::from(
                    "download Content-Length does not match Content-Range",
                )));
            }
            if resumed {
                if content_range.start != state.bytes_written {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "resumed download started at an unexpected byte",
                    )));
                }
                if response_etag.as_deref() != state.etag.as_deref() {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "resumed download returned a missing or different strong ETag",
                    )));
                }
                if state
                    .expected_total
                    .is_some_and(|expected| expected != total)
                {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "resumed download changed the complete representation length",
                    )));
                }
            } else if content_range.start != 0 || content_range.end.checked_add(1) != Some(total) {
                return Err(EngineError::InvalidResponse(ErrorDetail::from(
                    "partial response cannot be promoted as a complete download",
                )));
            } else {
                state.etag = response_etag;
            }
            state.expected_total = Some(total);
            Some(total)
        } else {
            if state.bytes_written > 0 {
                reset_download_state(state).await?;
            }
            state.etag = response_etag;
            state.expected_total = content_length;
            content_length
        };
        if expected_total.is_some_and(|length| length > target.max_bytes) {
            return Err(EngineError::FileTooLarge {
                limit_bytes: target.max_bytes,
            });
        }

        let attempt_start = state.bytes_written;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest_error)?;
            let chunk_length = u64::try_from(chunk.len()).map_err(|_| {
                EngineError::Runtime(ErrorDetail::from("download chunk length overflowed u64"))
            })?;
            if state.bytes_written.saturating_add(chunk_length) > target.max_bytes {
                return Err(EngineError::FileTooLarge {
                    limit_bytes: target.max_bytes,
                });
            }
            #[cfg(test)]
            crate::injection::fault(crate::injection::Point::Write).map_err(download_write_io)?;
            state
                .file
                .as_mut()
                .ok_or_else(|| {
                    EngineError::Runtime(ErrorDetail::from("download staging file is closed"))
                })?
                .write_all(&chunk)
                .await
                .map_err(download_write_io)?;
            state.digest.update(&chunk);
            state.bytes_written = state.bytes_written.saturating_add(chunk_length);
            state.bytes_received = state.bytes_received.saturating_add(chunk_length);
        }
        let attempt_bytes = state.bytes_written.saturating_sub(attempt_start);
        if content_length.is_some_and(|length| length != attempt_bytes) {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "download body length does not match Content-Length",
            )));
        }
        if expected_total.is_some_and(|total| state.bytes_written != total) {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "download body length does not match the complete representation",
            )));
        }
        let file = state.file.as_mut().ok_or_else(|| {
            EngineError::Runtime(ErrorDetail::from("download staging file is closed"))
        })?;
        #[cfg(test)]
        crate::injection::fault(crate::injection::Point::Flush).map_err(download_write_io)?;
        file.flush().await.map_err(download_write_io)?;
        file.sync_all().await.map_err(download_write_io)?;
        drop(state.file.take());

        #[cfg(test)]
        crate::injection::fault(crate::injection::Point::Finalize).map_err(download_write_io)?;
        let sha256 = state
            .digest
            .clone()
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if let Some(expected) = &target.expected_sha256
            && !sha256.eq_ignore_ascii_case(expected)
        {
            return Err(EngineError::ChecksumMismatch);
        }
        persist_download(&state.temporary, &target.path, target.overwrite).await?;
        state.cleanup = false;
        Ok(DownloadData {
            status,
            final_url,
            attempts: 1,
            network_requests,
            auth_requests: 0,
            auth_retries: 0,
            rate_limit_wait_ms,
            headers,
            bytes_written: state.bytes_written,
            bytes_received: state.bytes_received,
            sha256,
        })
    }
}

pub(super) async fn multipart_form(
    fields: &[(String, String)],
    files: &[PreparedFile],
) -> Result<Form, EngineError> {
    let mut form = Form::new();
    for (name, value) in fields {
        form = form.text(name.clone(), value.clone());
    }
    for file in files {
        let part = match &file.source {
            PreparedFileSource::Bytes(data) => Part::bytes(data.clone()),
            PreparedFileSource::Path { path, length } => {
                let source = fs::File::open(path).await.map_err(file_io)?;
                let body = Body::wrap_stream(ReaderStream::new(source));
                Part::stream_with_length(body, *length)
            }
        };
        let mut part = part.file_name(file.filename.clone());
        if let Some(content_type) = &file.content_type {
            part = part.mime_str(content_type).map_err(|_| {
                EngineError::InvalidInput(ErrorDetail::from("invalid multipart content type"))
            })?;
        }
        form = form.part(file.field_name.clone(), part);
    }
    Ok(form)
}

pub(super) async fn stream_body(
    builder: reqwest::RequestBuilder,
    stream: &PreparedStream,
) -> Result<reqwest::RequestBuilder, EngineError> {
    let source = fs::File::open(&stream.path).await.map_err(file_io)?;
    let body = Body::wrap_stream(ReaderStream::new(source));
    Ok(builder.header(CONTENT_LENGTH, stream.length).body(body))
}

fn response_content_length(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn strong_response_etag(response: &reqwest::Response) -> Option<String> {
    strong_etag(response.headers().get(ETAG)?.to_str().ok()?)
}

/// The strong validator carried by an `ETag` value, if it is one.
pub(crate) fn strong_etag(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with("W/")
        || value.len() < 2
        || !value.starts_with('"')
        || !value.ends_with('"')
    {
        None
    } else {
        Some(value.to_owned())
    }
}

pub(crate) struct ContentRange {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) total: u64,
}

fn satisfied_content_range(response: &reqwest::Response) -> Result<ContentRange, EngineError> {
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from(
                "206 response has no Content-Range header",
            ))
        })?
        .to_str()
        .map_err(|_| {
            EngineError::InvalidResponse(ErrorDetail::from(
                "download Content-Range is not valid text",
            ))
        })?;
    parse_content_range(value)
}

/// A satisfied `Content-Range` value: `bytes <start>-<end>/<total>`.
pub(crate) fn parse_content_range(value: &str) -> Result<ContentRange, EngineError> {
    let (unit, range_and_total) = value.trim().split_once(' ').ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range is malformed"))
    })?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "download Content-Range must use byte units",
        )));
    }
    let (range, total) = range_and_total.split_once('/').ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range is malformed"))
    })?;
    let (start, end) = range.split_once('-').ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from(
            "download Content-Range does not describe a satisfied range",
        ))
    })?;
    let start = start.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range start is invalid"))
    })?;
    let end = end.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range end is invalid"))
    })?;
    let total = total.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range total is invalid"))
    })?;
    if start > end || end >= total {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "download Content-Range bounds are invalid",
        )));
    }
    Ok(ContentRange { start, end, total })
}

fn download_temporary_path(target: &Path) -> PathBuf {
    let sequence = DOWNLOAD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let filename = target
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "download".into());
    target.with_file_name(format!(
        ".{filename}.rest-engine-{}-{sequence}.part",
        std::process::id()
    ))
}

async fn create_download_state(target: &Path) -> Result<DownloadState, EngineError> {
    // No `.await` between creating the staging file and arming its Drop guard:
    // a cancellation can only drop this future before the file exists or after
    // the guard owns it.
    let (temporary, file) = create_download_file(target)?;
    Ok(DownloadState {
        temporary,
        file: Some(file),
        bytes_written: 0,
        bytes_received: 0,
        digest: Sha256::new(),
        etag: None,
        expected_total: None,
        cleanup: true,
    })
}

async fn reset_download_state(state: &mut DownloadState) -> Result<(), EngineError> {
    drop(state.file.take());
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&state.temporary)
        .await
        // A retry of a download whose request already went out: the remote
        // side may have acted, so this is not a failure without effect.
        .map_err(download_write_io)?;
    state.file = Some(file);
    state.bytes_written = 0;
    state.digest = Sha256::new();
    state.etag = None;
    state.expected_total = None;
    Ok(())
}

async fn discard_download_state(state: &mut DownloadState) {
    drop(state.file.take());
    // Only disarm when the staging file is really gone; otherwise leave the
    // Drop guard to try again rather than leaking a partial download.
    if fs::remove_file(&state.temporary).await.is_ok() {
        state.cleanup = false;
    }
}

/// Creates the staging file synchronously.
///
/// `tokio::fs` runs the open on a blocking thread, and that thread finishes the
/// call even when the awaiting future is dropped: a download cancelled during
/// the open left a `.part` file that no guard owned. Creating an empty file is a
/// single short system call, so it is done inline instead, and the caller arms
/// the cleanup guard before yielding.
fn create_download_file(target: &Path) -> Result<(PathBuf, fs::File), EngineError> {
    #[cfg(test)]
    crate::injection::fault(crate::injection::Point::Open).map_err(file_io)?;
    for _ in 0..16 {
        let temporary = download_temporary_path(target);
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, fs::File::from_std(file))),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(file_io(error)),
        }
    }
    Err(EngineError::FileIo(ErrorDetail::from(
        "could not allocate a unique temporary download file",
    )))
}

async fn persist_download(
    temporary: &Path,
    target: &Path,
    overwrite: bool,
) -> Result<(), EngineError> {
    enter_phase(ErrorPhase::Write);
    if !overwrite {
        #[cfg(test)]
        crate::injection::fault(crate::injection::Point::Publish).map_err(download_write_io)?;
        fs::hard_link(temporary, target)
            .await
            .map_err(download_write_io)?;
        // The sink now holds the download: a failure from here on leaves the
        // publication in place and only the staging file behind.
        #[cfg(test)]
        crate::injection::fault(crate::injection::Point::Cleanup)
            .map_err(|error| EngineError::CleanupAfterPublish(crate::error::io_detail(&error)))?;
        fs::remove_file(temporary)
            .await
            .map_err(|error| EngineError::CleanupAfterPublish(crate::error::io_detail(&error)))?;
        return Ok(());
    }
    #[cfg(test)]
    crate::injection::fault(crate::injection::Point::Rename).map_err(download_write_io)?;
    match fs::rename(temporary, target).await {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
            ) && fs::try_exists(target).await.map_err(download_write_io)? =>
        {
            fs::remove_file(target).await.map_err(download_write_io)?;
            fs::rename(temporary, target)
                .await
                .map_err(download_write_io)
        }
        Err(error) => Err(download_write_io(error)),
    }
}

/// A local write of a download, after its HTTP request went out. The request
/// may have had a remote effect (a download is not restricted to safe
/// methods), so the failure cannot claim that nothing happened remotely.
fn download_write_io(error: std::io::Error) -> EngineError {
    EngineError::DownloadWrite(crate::error::io_detail(&error))
}
