use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::error::ErrorDetail;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{StreamExt, stream};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Url;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncReadExt, time::sleep};

use crate::{
    ASYNC_JOB_RECOVERY_CONTRACT, AsyncJobRecovery, AuthConfig, BatchConfig, BatchInputFormat,
    BodyType, CachePolicy, CancellationToken, CapabilityDocument, ConnectionConfig, CookiePolicy,
    CookieSession, EngineConfig, EngineError, ExecutionControl, ExecutionError, ExecutionMetrics,
    ExecutionOperation, ExecutionOutput, ExecutionRequest, ExecutionResult, ExecutionStatus,
    FileTransferDirection, FileTransferInput, HttpMethod, HttpResponseMetadata,
    IdempotencyLocation, IntegrityMetadata, JsonObject, OutputMapping, PaginationConfig,
    ParameterLocation, ParameterMode, PollingCancelConfig, PollingConfig, QuerySerialization,
    QueryStyle, ResponseConfig, ResponseTransform, SCHEMA_VERSION, capabilities, json_path,
    response_body,
    transport::{
        DownloadTarget, EXECUTION_TALLY, ExecutionTally, PreparedBody, PreparedFile,
        PreparedFileSource, PreparedRequest, PreparedStream, ResponseData, Transport, same_origin,
    },
};

pub struct Engine {
    config: EngineConfig,
    transport: Transport,
    closed: AtomicBool,
    idempotency: Mutex<IdempotencyRegistry>,
}

#[derive(Default)]
struct IdempotencyRegistry {
    fingerprints: HashMap<String, String>,
    insertion_order: VecDeque<String>,
}

#[derive(Clone)]
struct ActiveAsyncJob {
    recovery: Option<AsyncJobRecovery>,
    cancel: Option<ActiveRemoteCancel>,
}

#[derive(Clone)]
struct ActiveRemoteCancel {
    request: PreparedRequest,
    /// Kept so the revocation is re-checked when the request is actually sent.
    /// A cancellation is registered while polling and may fire much later, by
    /// which time a further hop may have revoked the authorization.
    scope: CredentialScope,
    on_cancellation: bool,
    on_deadline: bool,
    on_poll_timeout: bool,
}

#[derive(Clone, Copy)]
enum RemoteCancelTrigger {
    Cancellation,
    Deadline,
    PollTimeout,
}

tokio::task_local! {
    static ACTIVE_ASYNC_JOBS: Arc<Mutex<BTreeMap<String, ActiveAsyncJob>>>;
}

/// Total wall-clock allowance for best-effort remote cancellations issued after
/// a deadline or an explicit cancellation.
const REMOTE_CANCEL_BUDGET: Duration = Duration::from_secs(5);

/// Maximum remote cancellations issued concurrently within that budget.
const REMOTE_CANCEL_CONCURRENCY: usize = 8;

const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

struct OperationResult {
    output: ExecutionOutput,
    errors: Vec<ExecutionError>,
    succeeded: usize,
}

struct PollCompletion {
    value: Value,
    response: ResponseData,
    job_id: Option<Value>,
    active_key: Option<String>,
}

struct EnrichmentOutcome {
    index: usize,
    record: JsonObject,
    result: Result<Vec<JsonObject>, EngineError>,
    metrics: ExecutionMetrics,
    responses: Vec<HttpResponseMetadata>,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            transport: Transport::new(config.clone()),
            config,
            closed: AtomicBool::new(false),
            idempotency: Mutex::new(IdempotencyRegistry::default()),
        }
    }

    pub async fn execute(&self, request: ExecutionRequest) -> ExecutionResult {
        self.execute_with_control(request, ExecutionControl::default())
            .await
    }

    pub async fn execute_with_control(
        &self,
        request: ExecutionRequest,
        control: ExecutionControl,
    ) -> ExecutionResult {
        if self.is_closed() {
            return failed_result(EngineError::EngineClosed);
        }
        let control = match control.with_request_deadline(request.options.deadline.as_deref()) {
            Ok(control) => control,
            Err(error) => return failed_result(error),
        };
        if let Err(error) = validate_execution_configuration(&request) {
            return failed_result(error);
        }
        // Checked once for the whole operation, before any network activity
        // and before a credential scope can strip the session from a
        // follow-up: a stale handle must refuse the operation, not let parts
        // of it run without cookies.
        if let Err(error) = self
            .transport
            .check_cookie_session(request.connection.cookies.session.as_ref())
            .await
        {
            return failed_result(error);
        }
        if let Err(error) = self.admit_idempotency(&request) {
            return failed_result(error);
        }
        // An expired deadline refuses the operation before anything runs:
        // nothing was sent, so the failure is in validation with no remote
        // effect, unlike a deadline that fires during the execution.
        if control.deadline_expired() {
            return failed_result(EngineError::DeadlineExpired);
        }
        let active_jobs = Arc::new(Mutex::new(BTreeMap::new()));
        let deadline = control.deadline.map(tokio::time::Instant::from_std);
        let tally = Arc::new(ExecutionTally::default());
        let execution = EXECUTION_TALLY.scope(
            tally.clone(),
            ACTIVE_ASYNC_JOBS.scope(active_jobs.clone(), self.execute_inner(request)),
        );
        enum Controlled<T> {
            Finished(T),
            Cancelled,
            Deadline,
        }
        let outcome = match deadline {
            Some(deadline) => {
                tokio::select! {
                    biased;
                    _ = control.cancellation.cancelled() => Controlled::Cancelled,
                    _ = tokio::time::sleep_until(deadline) => Controlled::Deadline,
                    result = execution => Controlled::Finished(result),
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = control.cancellation.cancelled() => Controlled::Cancelled,
                    result = execution => Controlled::Finished(result),
                }
            }
        };
        let mut result = match outcome {
            Controlled::Finished(result) => result,
            Controlled::Cancelled => {
                // The remote cancellation is a request of this execution too.
                EXECUTION_TALLY
                    .scope(
                        tally.clone(),
                        self.cancel_active_jobs(&active_jobs, RemoteCancelTrigger::Cancellation),
                    )
                    .await;
                failed_result_with_recoveries(EngineError::Cancelled, recoveries_from(&active_jobs))
            }
            Controlled::Deadline => {
                // The remote cancellation is a request of this execution too.
                EXECUTION_TALLY
                    .scope(
                        tally.clone(),
                        self.cancel_active_jobs(&active_jobs, RemoteCancelTrigger::Deadline),
                    )
                    .await;
                failed_result_with_recoveries(EngineError::Timeout, recoveries_from(&active_jobs))
            }
        };
        // Never fewer requests or retries than were actually sent: the
        // response-based counts miss the attempts of a failure.
        result.metrics.requests = result
            .metrics
            .requests
            .max(tally.requests.load(Ordering::Relaxed));
        result.metrics.retries = result
            .metrics
            .retries
            .max(tally.retries.load(Ordering::Relaxed));
        result
    }

    pub fn capabilities(&self) -> CapabilityDocument {
        capabilities()
    }

    /// Opens a cookie session and returns its handle.
    ///
    /// A request uses the session by naming the handle in
    /// `connection.cookies.session`. The engine keeps at most
    /// `EngineConfig::max_cookie_sessions` sessions; opening one more evicts the
    /// least recently used session no operation is holding, and its handle is
    /// refused from then on.
    pub async fn open_cookie_session(&self) -> Result<CookieSession, EngineError> {
        if self.is_closed() {
            return Err(EngineError::EngineClosed);
        }
        self.transport.open_cookie_session().await
    }

    /// Closes a cookie session. Its handle, and every copy of it, is refused
    /// from now on; closing a stale handle is an error.
    pub async fn close_cookie_session(&self, session: &CookieSession) -> Result<(), EngineError> {
        if self.is_closed() {
            return Err(EngineError::EngineClosed);
        }
        self.transport.close_cookie_session(session).await
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn admit_idempotency(&self, request: &ExecutionRequest) -> Result<(), EngineError> {
        let Some(key) = request.options.idempotency_key.as_deref() else {
            return Ok(());
        };
        validate_idempotency_key(key)?;
        if self.config.max_idempotency_keys == 0 {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "max_idempotency_keys must be greater than zero when a key is used",
            )));
        }
        let fingerprint = execution_fingerprint(request)?;
        let mut registry = self
            .idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = registry.fingerprints.get(key) {
            return if existing == &fingerprint {
                Ok(())
            } else {
                Err(EngineError::IdempotencyConflict)
            };
        }
        while registry.fingerprints.len() >= self.config.max_idempotency_keys {
            let Some(oldest) = registry.insertion_order.pop_front() else {
                break;
            };
            registry.fingerprints.remove(&oldest);
        }
        registry.fingerprints.insert(key.to_owned(), fingerprint);
        registry.insertion_order.push_back(key.to_owned());
        Ok(())
    }

    async fn cancel_active_jobs(
        &self,
        jobs: &Arc<Mutex<BTreeMap<String, ActiveAsyncJob>>>,
        trigger: RemoteCancelTrigger,
    ) {
        let requests = {
            let mut jobs = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.iter_mut()
                .filter_map(|(key, job)| {
                    let cancel = job.cancel.as_ref()?;
                    if !cancel_enabled(cancel, trigger) {
                        return None;
                    }
                    if let Some(recovery) = job.recovery.as_mut() {
                        recovery.cancel_requested = true;
                    }
                    // Re-applied here, not only when the job was registered: a
                    // later hop may have revoked the authorization since.
                    let mut request = cancel.request.clone();
                    cancel.scope.apply(&mut request, None);
                    Some((key.clone(), request))
                })
                .collect::<Vec<_>>()
        };
        // Best-effort cleanup after a deadline or a cancellation must not extend
        // the very deadline it is reacting to: the remote cancellations run with
        // bounded concurrency inside a single global budget, and whatever does
        // not finish in time is simply left unreported.
        let cleanup = stream::iter(requests)
            .map(|(key, request)| {
                let jobs = jobs.clone();
                async move {
                    let accepted = self
                        .transport
                        .execute(request)
                        .await
                        .is_ok_and(|response| (200..300).contains(&response.status));
                    let mut jobs = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(recovery) = jobs.get_mut(&key).and_then(|job| job.recovery.as_mut())
                    {
                        recovery.cancel_accepted = Some(accepted);
                    }
                }
            })
            .buffer_unordered(REMOTE_CANCEL_CONCURRENCY)
            .collect::<Vec<()>>();
        let _ = tokio::time::timeout(REMOTE_CANCEL_BUDGET, cleanup).await;
    }

    async fn cancel_active_job(&self, key: &str, trigger: RemoteCancelTrigger) {
        let Some(jobs) = active_jobs_handle() else {
            return;
        };
        let request = {
            let mut locked = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(job) = locked.get_mut(key) else {
                return;
            };
            let Some(cancel) = job.cancel.as_ref() else {
                return;
            };
            if !cancel_enabled(cancel, trigger) {
                return;
            }
            if let Some(recovery) = job.recovery.as_mut() {
                recovery.cancel_requested = true;
            }
            let mut request = cancel.request.clone();
            cancel.scope.apply(&mut request, None);
            request
        };
        // Same budget as the deadline and cancellation paths: a poll that has
        // already exhausted `max_wait_ms` must not then block indefinitely on a
        // best-effort remote cancellation. On expiry the recovery handle keeps
        // `cancel_accepted = None`, which is exactly "requested, outcome
        // unknown".
        let Ok(accepted) =
            tokio::time::timeout(REMOTE_CANCEL_BUDGET, self.transport.execute(request)).await
        else {
            return;
        };
        let accepted = accepted.is_ok_and(|response| (200..300).contains(&response.status));
        let mut locked = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(recovery) = locked.get_mut(key).and_then(|job| job.recovery.as_mut()) {
            recovery.cancel_accepted = Some(accepted);
        }
    }

    async fn execute_inner(&self, request: ExecutionRequest) -> ExecutionResult {
        let started = Instant::now();
        let mut metrics = ExecutionMetrics {
            input_records: request.input.records.len(),
            ..ExecutionMetrics::default()
        };
        let mut responses = Vec::new();

        let result = if request.connection.credential_ref.is_some() {
            Err(EngineError::InvalidInput(ErrorDetail::from(
                "credential_ref requires an authorized runtime resolver",
            )))
        } else if request.schema_version != SCHEMA_VERSION {
            Err(EngineError::UnsupportedSchema {
                received: request.schema_version,
                supported: SCHEMA_VERSION,
            })
        } else {
            match request.operation {
                ExecutionOperation::Test => self.test(&request, &mut metrics, &mut responses).await,
                ExecutionOperation::Generate => {
                    self.generate(&request, &mut metrics, &mut responses).await
                }
                ExecutionOperation::Enrich => {
                    self.enrich(&request, &mut metrics, &mut responses).await
                }
                ExecutionOperation::Download => {
                    self.download(&request, &mut metrics, &mut responses).await
                }
                ExecutionOperation::Upload => {
                    self.upload(&request, &mut metrics, &mut responses).await
                }
            }
        };

        metrics.elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        match result {
            Ok(operation) => {
                metrics.output_records = operation.succeeded;
                let status = if operation.errors.is_empty() {
                    ExecutionStatus::Success
                } else if operation.succeeded > 0 {
                    ExecutionStatus::Partial
                } else {
                    ExecutionStatus::Failed
                };
                with_recoveries(
                    ExecutionResult {
                        schema_version: SCHEMA_VERSION,
                        status,
                        output: operation.output,
                        metrics,
                        responses,
                        errors: operation.errors,
                        recoveries: Vec::new(),
                    },
                    active_recoveries(),
                )
            }
            Err(error) => with_recoveries(
                ExecutionResult {
                    schema_version: SCHEMA_VERSION,
                    status: ExecutionStatus::Failed,
                    output: ExecutionOutput::None,
                    metrics,
                    responses,
                    errors: vec![error.execution_error(None)],
                    recoveries: Vec::new(),
                },
                active_recoveries(),
            ),
        }
    }

    pub async fn execute_json(&self, request_json: &str) -> Result<String, EngineError> {
        self.execute_json_with_cancellation(request_json, CancellationToken::new())
            .await
    }

    pub async fn execute_json_with_cancellation(
        &self,
        request_json: &str,
        cancellation: CancellationToken,
    ) -> Result<String, EngineError> {
        if self.is_closed() {
            return Err(EngineError::EngineClosed);
        }
        let request = serde_json::from_str::<ExecutionRequest>(request_json).map_err(|error| {
            EngineError::InvalidInput(ErrorDetail::at(
                "request is not valid JSON for the contract",
                error.line(),
                error.column(),
            ))
        })?;
        let result = self
            .execute_with_control(request, ExecutionControl::new(cancellation))
            .await;
        serde_json::to_string(&result)
            .map_err(|_| EngineError::Runtime(ErrorDetail::from("result could not be serialized")))
    }

    async fn test(
        &self,
        request: &ExecutionRequest,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
    ) -> Result<OperationResult, EngineError> {
        let parameters = resolve_parameters(&request.connection, &request.input.params)?;
        let (value, _, _, _) = self
            .request_json(
                &request.connection,
                &parameters,
                None,
                metrics,
                responses,
                &request.options,
            )
            .await?;
        Ok(OperationResult {
            output: ExecutionOutput::Json { value },
            errors: Vec::new(),
            succeeded: 1,
        })
    }

    async fn download(
        &self,
        request: &ExecutionRequest,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
    ) -> Result<OperationResult, EngineError> {
        let file = required_file_input(request)?;
        let target_path = self.resolve_download_target(file).await?;
        let target = DownloadTarget {
            path: target_path,
            overwrite: file.overwrite,
            resume: file.resume,
            max_bytes: transfer_limit(&self.config, file)?,
            expected_sha256: validated_checksum(file.expected_sha256.as_deref())?,
        };
        let parameters = resolve_parameters(&request.connection, &request.input.params)?;
        let initial = self.prepare_request(
            &request.connection,
            &parameters,
            None,
            request.options.idempotency_key.as_deref(),
        )?;
        let mut credential_scope = CredentialScope::new(initial.url.clone());
        let (prepared, is_poll_result, active_key) = match &request.connection.polling {
            Some(polling) => {
                let completion = if polling.resume.is_some() {
                    self.await_resumed_poll_completion(
                        &request.connection,
                        polling,
                        &mut credential_scope,
                        metrics,
                    )
                    .await?
                } else {
                    let (initial_value, initial_response) = self
                        .execute_prepared_json(&request.connection, initial, metrics, false)
                        .await?;
                    self.await_poll_completion(
                        &request.connection,
                        polling,
                        initial_value,
                        initial_response,
                        &mut credential_scope,
                        metrics,
                    )
                    .await?
                };
                let prepared = self.prepare_poll_result_request(
                    &request.connection,
                    polling,
                    &completion.value,
                    &completion.response,
                    completion.job_id.as_ref(),
                    &mut credential_scope,
                )?;
                (prepared, true, completion.active_key)
            }
            None => (initial, false, None),
        };
        let response = self
            .transport
            .download(prepared, &target, &request.connection.success_statuses)
            .await?;
        if let Some(key) = active_key {
            remove_active_job(&key);
        }
        metrics.requests = metrics.requests.saturating_add(response.network_requests);
        metrics.retries = metrics
            .retries
            .saturating_add(u64::from(response.attempts.saturating_sub(1)))
            .saturating_add(response.auth_retries);
        metrics.auth_requests = metrics.auth_requests.saturating_add(response.auth_requests);
        metrics.rate_limit_wait_ms = metrics
            .rate_limit_wait_ms
            .saturating_add(response.rate_limit_wait_ms);
        metrics.bytes_downloaded = metrics
            .bytes_downloaded
            .saturating_add(response.bytes_received);
        if is_poll_result {
            metrics.poll_requests = metrics.poll_requests.saturating_add(
                response
                    .network_requests
                    .saturating_sub(response.auth_requests),
            );
        }
        if request.options.capture_response_metadata {
            responses.push(HttpResponseMetadata {
                status: response.status,
                final_url: public_response_url(&response.final_url),
                attempts: response.attempts,
                headers: selected_response_headers(
                    &response.headers,
                    &request.options.response_headers,
                ),
            });
        }
        Ok(OperationResult {
            output: ExecutionOutput::File {
                direction: FileTransferDirection::Download,
                artifact_reference: file
                    .artifact_sink
                    .as_ref()
                    .map(|artifact| artifact.reference.clone())
                    .unwrap_or_else(|| format!("sha256:{}", response.sha256)),
                bytes_transferred: response.bytes_written,
                checksum: IntegrityMetadata {
                    algorithm: "sha256".to_owned(),
                    value: response.sha256,
                },
                media_type: response.headers.get("content-type").cloned(),
                response: None,
            },
            errors: Vec::new(),
            succeeded: 1,
        })
    }

    async fn upload(
        &self,
        request: &ExecutionRequest,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
    ) -> Result<OperationResult, EngineError> {
        let file = required_file_input(request)?;
        let source = self.resolve_upload_source(file).await?;
        let limit = transfer_limit(&self.config, file)?;
        let metadata = fs::metadata(&source).await.map_err(file_io)?;
        if !metadata.is_file() {
            return Err(EngineError::FileIo(ErrorDetail::from(
                "upload source is not a regular file",
            )));
        }
        let length = metadata.len();
        if length > limit {
            return Err(EngineError::FileTooLarge { limit_bytes: limit });
        }
        // The transport reopens the file by path for every send, redirect, and
        // retry, so the hashed bytes and the transmitted bytes are only the same
        // as long as nothing rewrites the file underneath us. The digest is
        // therefore recomputed once the transfer is done: comparing content
        // rather than size and timestamps detects a same-size rewrite, a
        // restored mtime, and a replacement through rename, none of which
        // metadata alone would reveal.
        let sha256 = hash_file(&source, limit).await?;
        if let Some(expected) = validated_checksum(file.expected_sha256.as_deref())? {
            if !sha256.eq_ignore_ascii_case(&expected) {
                return Err(EngineError::ChecksumMismatch);
            }
        }

        let parameters = resolve_parameters(&request.connection, &request.input.params)?;
        let mut prepared = self.prepare_request(
            &request.connection,
            &parameters,
            None,
            request.options.idempotency_key.as_deref(),
        )?;
        let content_type = file.content_type.clone();
        match &mut prepared.body {
            body @ PreparedBody::Raw(_) => {
                *body = PreparedBody::Stream(PreparedStream {
                    path: source.clone(),
                    length,
                    content_type,
                });
            }
            PreparedBody::Multipart { files, .. } => {
                validate_multipart_name(&file.field_name)?;
                let filename = file
                    .filename
                    .clone()
                    .or_else(|| {
                        source
                            .file_name()
                            .map(|value| value.to_string_lossy().into_owned())
                    })
                    .ok_or_else(|| {
                        EngineError::InvalidInput(ErrorDetail::from(
                            "upload file requires an explicit filename",
                        ))
                    })?;
                validate_multipart_name(&filename)?;
                files.push(PreparedFile {
                    field_name: file.field_name.clone(),
                    filename,
                    content_type,
                    source: PreparedFileSource::Path {
                        path: source.clone(),
                        length,
                    },
                });
            }
            _ => {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "upload requires request.body_type 'raw' or 'multipart'",
                )));
            }
        }
        let (value, _, _, _) = self
            .request_prepared_json(
                &request.connection,
                prepared,
                None,
                metrics,
                responses,
                &request.options,
            )
            .await?;
        // Reported as a checksum mismatch rather than an I/O error on purpose:
        // this check runs *after* the remote request completed, so the contract
        // must say the remote effect is unknown and the failure belongs to the
        // finalize phase. `FileIo` would claim `remote_effect: none` and tell
        // the caller nothing happened remotely, which is exactly wrong here.
        let transferred = hash_file(&source, limit)
            .await
            .map_err(|_| EngineError::ChecksumMismatch)?;
        if transferred != sha256 {
            return Err(EngineError::ChecksumMismatch);
        }
        metrics.bytes_uploaded = metrics.bytes_uploaded.saturating_add(length);
        Ok(OperationResult {
            output: ExecutionOutput::File {
                direction: FileTransferDirection::Upload,
                artifact_reference: file
                    .artifact_source
                    .as_ref()
                    .map(|artifact| artifact.reference.clone())
                    .unwrap_or_else(|| format!("sha256:{sha256}")),
                bytes_transferred: length,
                checksum: IntegrityMetadata {
                    algorithm: "sha256".to_owned(),
                    value: sha256,
                },
                media_type: file.content_type.clone(),
                response: Some(value),
            },
            errors: Vec::new(),
            succeeded: 1,
        })
    }

    async fn generate(
        &self,
        request: &ExecutionRequest,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
    ) -> Result<OperationResult, EngineError> {
        let parameters = resolve_parameters(&request.connection, &request.input.params)?;
        let mut limit = None;
        let values = match &request.connection.pagination {
            None => {
                let (value, _, _, _) = self
                    .request_json(
                        &request.connection,
                        &parameters,
                        None,
                        metrics,
                        responses,
                        &request.options,
                    )
                    .await?;
                response_records(&request.connection, &value)?
            }
            Some(pagination) => {
                let (values, stopped) = self
                    .paginated(
                        &request.connection,
                        &parameters,
                        pagination,
                        metrics,
                        responses,
                        &request.options,
                    )
                    .await?;
                limit = stopped;
                values
            }
        };
        let records = map_records(values, &request.connection.response)?;
        let succeeded = records.len();
        // Rows cut off by a pagination limit are not a complete result: the
        // records read so far are returned with an error saying so, which
        // makes the status partial (or failed with no rows).
        Ok(OperationResult {
            output: ExecutionOutput::Records { records },
            errors: limit
                .map(|error| vec![error.execution_error(None)])
                .unwrap_or_default(),
            succeeded,
        })
    }

    async fn enrich(
        &self,
        request: &ExecutionRequest,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
    ) -> Result<OperationResult, EngineError> {
        if let Some(batch) = request
            .connection
            .batch
            .as_ref()
            .filter(|batch| batch.enabled)
        {
            return self.enrich_batch(request, batch, metrics, responses).await;
        }
        if request.options.enrichment_concurrency == 0 {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "enrichment_concurrency must be greater than zero",
            )));
        }
        if request.options.enrichment_concurrency > 1 && request.options.continue_on_error {
            return self.enrich_concurrent(request, metrics, responses).await;
        }
        let mut output = Vec::with_capacity(request.input.records.len());
        let mut errors = Vec::new();
        let mut succeeded = 0;

        for (index, record) in request.input.records.iter().enumerate() {
            let scoped_options = scoped_execution_options(&request.options, index);
            let mut source = request.input.params.clone();
            source.extend(record.clone());
            let result = match resolve_parameters(&request.connection, &source) {
                Ok(parameters) => self
                    .request_json(
                        &request.connection,
                        &parameters,
                        None,
                        metrics,
                        responses,
                        &scoped_options,
                    )
                    .await
                    .map(|(value, _, _, _)| value)
                    .and_then(|value| {
                        response_records(&request.connection, &value)
                            .and_then(|values| map_records(values, &request.connection.response))
                    }),
                Err(error) => Err(error),
            };

            match result {
                Ok(additions) => {
                    if additions.is_empty() {
                        output.push(record.clone());
                        succeeded += 1;
                    } else {
                        for additions in additions {
                            let mut enriched = record.clone();
                            enriched.extend(additions);
                            output.push(enriched);
                            succeeded += 1;
                        }
                    }
                }
                Err(error) => {
                    output.push(record.clone());
                    errors.push(error.execution_error(Some(index)));
                    if !request.options.continue_on_error {
                        break;
                    }
                }
            }
        }

        Ok(OperationResult {
            output: ExecutionOutput::Records { records: output },
            errors,
            succeeded,
        })
    }

    async fn enrich_concurrent(
        &self,
        request: &ExecutionRequest,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
    ) -> Result<OperationResult, EngineError> {
        let concurrency = request.options.enrichment_concurrency;
        let outcomes = stream::iter(request.input.records.iter().cloned().enumerate().map(
            |(index, record)| async move {
                let mut local_metrics = ExecutionMetrics::default();
                let mut local_responses = Vec::new();
                let scoped_options = scoped_execution_options(&request.options, index);
                let mut source = request.input.params.clone();
                source.extend(record.clone());
                let result = match resolve_parameters(&request.connection, &source) {
                    Ok(parameters) => self
                        .request_json(
                            &request.connection,
                            &parameters,
                            None,
                            &mut local_metrics,
                            &mut local_responses,
                            &scoped_options,
                        )
                        .await
                        .map(|(value, _, _, _)| value)
                        .and_then(|value| {
                            response_records(&request.connection, &value).and_then(|values| {
                                map_records(values, &request.connection.response)
                            })
                        }),
                    Err(error) => Err(error),
                };
                EnrichmentOutcome {
                    index,
                    record,
                    result,
                    metrics: local_metrics,
                    responses: local_responses,
                }
            },
        ))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

        // Every record yields exactly one outcome at its own index. A missing,
        // repeated or out-of-range outcome would silently drop or duplicate a
        // record, so it fails the operation instead.
        let mut ordered: Vec<Option<EnrichmentOutcome>> =
            (0..request.input.records.len()).map(|_| None).collect();
        for outcome in outcomes {
            let slot = ordered.get_mut(outcome.index).ok_or_else(|| {
                EngineError::Runtime(ErrorDetail::from(
                    "enrichment outcome index is out of range",
                ))
            })?;
            if slot.replace(outcome).is_some() {
                return Err(EngineError::Runtime(ErrorDetail::from(
                    "enrichment record produced more than one outcome",
                )));
            }
        }
        let ordered = ordered
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                EngineError::Runtime(ErrorDetail::from("enrichment record produced no outcome"))
            })?;

        let mut output = Vec::with_capacity(request.input.records.len());
        let mut errors = Vec::new();
        let mut succeeded = 0_usize;
        for outcome in ordered {
            merge_execution_metrics(metrics, &outcome.metrics);
            responses.extend(outcome.responses);
            match outcome.result {
                Ok(additions) if additions.is_empty() => {
                    output.push(outcome.record);
                    succeeded = succeeded.saturating_add(1);
                }
                Ok(additions) => {
                    for additions in additions {
                        let mut enriched = outcome.record.clone();
                        enriched.extend(additions);
                        output.push(enriched);
                        succeeded = succeeded.saturating_add(1);
                    }
                }
                Err(error) => {
                    output.push(outcome.record);
                    errors.push(error.execution_error(Some(outcome.index)));
                }
            }
        }

        Ok(OperationResult {
            output: ExecutionOutput::Records { records: output },
            errors,
            succeeded,
        })
    }

    async fn enrich_batch(
        &self,
        request: &ExecutionRequest,
        batch: &BatchConfig,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
    ) -> Result<OperationResult, EngineError> {
        if batch.max_size == 0 {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "batch max_size must be greater than zero",
            )));
        }
        let mut connection = request.connection.clone();
        connection.batch = None;
        connection.pagination = None;
        if let Some(url) = &batch.endpoint_override {
            connection.url = url.clone();
        }
        if let Some(method) = &batch.method_override {
            connection.method = method.clone();
        }

        let mut output = Vec::with_capacity(request.input.records.len());
        let mut errors = Vec::new();
        let mut succeeded = 0_usize;
        for (chunk_index, records) in request.input.records.chunks(batch.max_size).enumerate() {
            let scoped_options = scoped_execution_options(&request.options, chunk_index);
            let base_index = chunk_index.saturating_mul(batch.max_size);
            let mut parameters = Vec::new();
            let mut valid = Vec::new();
            let mut resolution_errors = Vec::new();
            for (offset, record) in records.iter().enumerate() {
                let mut source = request.input.params.clone();
                source.extend(record.clone());
                let resolved = resolve_parameters(&connection, &source).and_then(|value| {
                    if batch.input_format == BatchInputFormat::FlatArray {
                        flat_batch_item(&value)?;
                    }
                    Ok(value)
                });
                match resolved {
                    Ok(value) => {
                        parameters.push(value);
                        valid.push(offset);
                    }
                    Err(error) => resolution_errors.push((offset, error)),
                }
            }

            let batch_result = if parameters.is_empty() {
                Ok(Vec::new())
            } else {
                let payload = batch_payload(batch, parameters);
                self.request_json(
                    &connection,
                    &payload,
                    None,
                    metrics,
                    responses,
                    &scoped_options,
                )
                .await
                .and_then(|(value, _, _, _)| batch_response(&value, batch, &connection.response))
            };
            let mapped = match batch_result {
                Ok(mapped) if mapped.len() == valid.len() => Some(mapped),
                Ok(_) => {
                    let error = EngineError::InvalidResponse(ErrorDetail::from(
                        "batch returned a different number of results than input records",
                    ));
                    for offset in &valid {
                        errors.push(error.execution_error(Some(base_index + offset)));
                    }
                    None
                }
                Err(error) => {
                    for offset in &valid {
                        errors.push(error.execution_error(Some(base_index + offset)));
                    }
                    None
                }
            };

            let mut mapped = mapped.map(IntoIterator::into_iter);
            for (offset, record) in records.iter().enumerate() {
                if let Some((_, error)) = resolution_errors
                    .iter()
                    .find(|(error_offset, _)| *error_offset == offset)
                {
                    errors.push(error.execution_error(Some(base_index + offset)));
                    output.push(record.clone());
                    continue;
                }
                let Some(additions) = mapped.as_mut().and_then(Iterator::next) else {
                    output.push(record.clone());
                    continue;
                };
                let mut enriched = record.clone();
                enriched.extend(additions);
                output.push(enriched);
                succeeded = succeeded.saturating_add(1);
            }
            if !request.options.continue_on_error && !errors.is_empty() {
                break;
            }
        }
        Ok(OperationResult {
            output: ExecutionOutput::Records { records: output },
            errors,
            succeeded,
        })
    }

    async fn paginated(
        &self,
        connection: &ConnectionConfig,
        base_parameters: &JsonObject,
        pagination: &PaginationConfig,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Vec<Value>, Option<EngineError>), EngineError> {
        let mut output = Vec::new();
        // Set when the source itself says it has no more data: a short page,
        // no next cursor or link, or one already followed. Leaving a loop for
        // any other reason (max_rows, max_pages), or dropping rows of the last
        // page to fit max_rows, means the source still had data.
        let mut finished = false;
        let mut dropped = false;
        // Origin that owns the credentials for the whole pagination run. It is
        // fixed by the first page and never re-derived, so no later page can
        // become the origin that owns them. Cursor, offset, and page values are
        // remote input too: `prepare_request` substitutes placeholders anywhere
        // in the URL, including the host, so every mode is scoped, not only the
        // ones that follow an explicit link.
        let mut credential_scope: Option<CredentialScope> = None;

        match pagination {
            PaginationConfig::Offset {
                offset_param,
                limit_param,
                page_size,
                max_rows,
                start,
            } => {
                ensure_page_size(*page_size)?;
                let mut offset = *start;
                let mut request_index = 0_usize;
                while output.len() < *max_rows {
                    let limit = (*page_size).min(max_rows.saturating_sub(output.len()));
                    let mut parameters = base_parameters.clone();
                    parameters.insert(offset_param.clone(), usize_value(offset)?);
                    parameters.insert(limit_param.clone(), usize_value(limit)?);
                    let scoped_options = scoped_execution_options(options, request_index);
                    let (value, _, _, _) = self
                        .request_page(
                            connection,
                            &parameters,
                            None,
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    let page = response_records(connection, &value)?;
                    let page_len = page.len();
                    dropped |= append_limited(&mut output, page, *max_rows);
                    if page_len < limit {
                        finished = true;
                        break;
                    }
                    offset = offset.saturating_add(*page_size);
                    request_index = request_index.saturating_add(1);
                }
            }
            PaginationConfig::Page {
                page_param,
                page_size_param,
                page_size,
                max_rows,
                start_page,
            } => {
                ensure_page_size(*page_size)?;
                let mut page_number = *start_page;
                let mut request_index = 0_usize;
                while output.len() < *max_rows {
                    // The page size stays the same on every request: page N
                    // of a smaller size is a different slice of the data
                    // (page 2 of size 1 is the second row, not the third).
                    // Rows beyond max_rows are cut locally instead.
                    let limit = *page_size;
                    let mut parameters = base_parameters.clone();
                    parameters.insert(page_param.clone(), usize_value(page_number)?);
                    parameters.insert(page_size_param.clone(), usize_value(limit)?);
                    let scoped_options = scoped_execution_options(options, request_index);
                    let (value, _, _, _) = self
                        .request_page(
                            connection,
                            &parameters,
                            None,
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    let page = response_records(connection, &value)?;
                    let page_len = page.len();
                    dropped |= append_limited(&mut output, page, *max_rows);
                    if page_len < limit {
                        finished = true;
                        break;
                    }
                    page_number = page_number.saturating_add(1);
                    request_index = request_index.saturating_add(1);
                }
            }
            PaginationConfig::Cursor {
                cursor_param,
                cursor_path,
                max_rows,
                max_pages,
            } => {
                let mut cursor: Option<String> = None;
                let mut seen = HashSet::new();
                for page_index in 0..*max_pages {
                    if output.len() >= *max_rows {
                        break;
                    }
                    let mut parameters = base_parameters.clone();
                    if let Some(cursor) = &cursor {
                        parameters.insert(cursor_param.clone(), Value::String(cursor.clone()));
                    }
                    let scoped_options = scoped_execution_options(options, page_index);
                    let (value, _, _, _) = self
                        .request_page(
                            connection,
                            &parameters,
                            None,
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    dropped |= append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    cursor = json_path::get(&value, cursor_path).and_then(value_as_string);
                    match &cursor {
                        Some(value) if seen.insert(value.clone()) => {}
                        _ => {
                            finished = true;
                            break;
                        }
                    }
                }
            }
            PaginationConfig::Link {
                link_path,
                max_rows,
                max_pages,
                allow_cross_origin,
            } => {
                let mut next_url: Option<String> = None;
                let mut seen = HashSet::new();
                for page_index in 0..*max_pages {
                    if output.len() >= *max_rows {
                        break;
                    }
                    let empty = JsonObject::new();
                    let parameters = if page_index == 0 {
                        base_parameters
                    } else {
                        &empty
                    };
                    let scoped_options = scoped_execution_options(options, page_index);
                    let (value, final_url, _, _) = self
                        .request_page(
                            connection,
                            parameters,
                            next_url.as_deref(),
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    dropped |= append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    let Some(link) = json_path::get(&value, link_path).and_then(Value::as_str)
                    else {
                        finished = true;
                        break;
                    };
                    let resolved =
                        pagination_url(&final_url, link, *allow_cross_origin)?.to_string();
                    if !seen.insert(resolved.clone()) {
                        finished = true;
                        break;
                    }
                    next_url = Some(resolved);
                }
            }
            PaginationConfig::HeaderLink {
                relation,
                max_rows,
                max_pages,
                allow_cross_origin,
            } => {
                if relation.trim().is_empty() {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "pagination Link relation cannot be empty",
                    )));
                }
                let mut next_url: Option<String> = None;
                let mut seen = HashSet::new();
                for page_index in 0..*max_pages {
                    if output.len() >= *max_rows {
                        break;
                    }
                    let empty = JsonObject::new();
                    let parameters = if page_index == 0 {
                        base_parameters
                    } else {
                        &empty
                    };
                    let scoped_options = scoped_execution_options(options, page_index);
                    let (value, final_url, _, headers) = self
                        .request_page(
                            connection,
                            parameters,
                            next_url.as_deref(),
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    dropped |= append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    let Some(link) = link_header_target(&headers, relation)? else {
                        finished = true;
                        break;
                    };
                    let resolved =
                        pagination_url(&final_url, &link, *allow_cross_origin)?.to_string();
                    if !seen.insert(resolved.clone()) {
                        finished = true;
                        break;
                    }
                    next_url = Some(resolved);
                }
            }
        }

        let stopped = (dropped || !finished).then(|| pagination_limit(pagination));
        Ok((output, stopped))
    }

    async fn request_json(
        &self,
        connection: &ConnectionConfig,
        parameters: &JsonObject,
        url_override: Option<&str>,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>), EngineError> {
        let request = self.prepare_request(
            connection,
            parameters,
            url_override,
            options.idempotency_key.as_deref(),
        )?;
        self.request_prepared_json(connection, request, None, metrics, responses, options)
            .await
    }

    /// Pagination step.
    ///
    /// `credential_scope` is fixed by the URL the first page is *sent to* and
    /// then reused unchanged for every later page and for every follow-up
    /// derived from them. It is deliberately not taken from a response: the URL
    /// a page finishes on may already be a polling or result URL the remote
    /// service chose, and letting that become the owning origin would hand the
    /// credentials to it on the next page.
    #[allow(clippy::too_many_arguments)]
    async fn request_page(
        &self,
        connection: &ConnectionConfig,
        parameters: &JsonObject,
        url_override: Option<&str>,
        credential_scope: &mut Option<CredentialScope>,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>), EngineError> {
        let mut request = self.prepare_request(
            connection,
            parameters,
            url_override,
            options.idempotency_key.as_deref(),
        )?;
        // The owning origin is the URL the first page is sent to, and the
        // authorization only ever narrows: a page served by another origin
        // revokes it for every later page as well.
        let scope =
            credential_scope.get_or_insert_with(|| CredentialScope::new(request.url.clone()));
        scope.narrow(&request.url);
        scope.apply(
            &mut request,
            preserved_idempotency_header(connection, options),
        );
        // The idempotency key is what switched on retries for a non-idempotent
        // method. When its header had to be withheld from this origin, the
        // request is no longer protected, so it is retried only if the caller
        // asked for that independently of the key.
        if idempotency_header_withheld(connection, options, &request) {
            request.retry.retry_non_idempotent = connection.retry.retry_non_idempotent;
        }
        self.request_prepared_json(
            connection,
            request,
            Some(scope),
            metrics,
            responses,
            options,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_prepared_json(
        &self,
        connection: &ConnectionConfig,
        request: PreparedRequest,
        credential_scope: Option<&mut CredentialScope>,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>), EngineError> {
        // Origin that owns the credentials carried by this request; every
        // follow-up leaving it must not take them along.
        //
        // An explicit origin always wins: deriving it from `request.url` would
        // re-anchor the scope to a URL a remote service chose, so a page already
        // stripped of credentials could hand them back to its own origin through
        // a polling follow-up.
        // An explicit scope is borrowed, not copied, so a restriction applied
        // while following the polling chain is visible to the caller afterwards.
        let mut owned_scope;
        let credential_scope = match credential_scope {
            Some(scope) => scope,
            None => {
                owned_scope = CredentialScope::new(request.url.clone());
                &mut owned_scope
            }
        };
        let result = match &connection.polling {
            Some(polling) if polling.resume.is_some() => {
                self.resume_poll(connection, polling, credential_scope, metrics)
                    .await
            }
            polling => {
                let (initial_value, initial_response) = self
                    .execute_prepared_json(connection, request, metrics, false)
                    .await?;
                match polling {
                    Some(polling) => {
                        self.poll(
                            connection,
                            polling,
                            initial_value,
                            initial_response,
                            credential_scope,
                            metrics,
                        )
                        .await
                    }
                    None => Ok((
                        initial_value,
                        initial_response.final_url,
                        initial_response.attempts,
                        initial_response.headers,
                        initial_response.status,
                    )),
                }
            }
        }?;
        evaluate_application_success(&result.0, connection)?;
        if options.capture_response_metadata {
            responses.push(HttpResponseMetadata {
                status: result.4,
                final_url: public_response_url(&result.1),
                attempts: result.2,
                headers: selected_response_headers(&result.3, &options.response_headers),
            });
        }
        Ok((result.0, result.1, result.2, result.3))
    }

    async fn execute_prepared_json(
        &self,
        connection: &ConnectionConfig,
        request: PreparedRequest,
        metrics: &mut ExecutionMetrics,
        is_poll: bool,
    ) -> Result<(Value, ResponseData), EngineError> {
        let response = self.transport.execute(request).await?;
        metrics.requests = metrics.requests.saturating_add(response.network_requests);
        metrics.retries = metrics
            .retries
            .saturating_add(u64::from(response.attempts.saturating_sub(1)))
            .saturating_add(response.auth_retries);
        metrics.auth_requests = metrics.auth_requests.saturating_add(response.auth_requests);
        metrics.cache_hits = metrics.cache_hits.saturating_add(response.cache_hits);
        metrics.cache_revalidations = metrics
            .cache_revalidations
            .saturating_add(response.cache_revalidations);
        metrics.rate_limit_wait_ms = metrics
            .rate_limit_wait_ms
            .saturating_add(response.rate_limit_wait_ms);
        if is_poll {
            metrics.poll_requests = metrics.poll_requests.saturating_add(
                response
                    .network_requests
                    .saturating_sub(response.auth_requests),
            );
        }
        ensure_success(connection, &response)?;

        let value = if response.body.is_empty() {
            Value::Null
        } else {
            response_body::parse(&response.body, &connection.response)?
        };
        Ok((value, response))
    }

    async fn poll(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        initial_value: Value,
        initial_response: ResponseData,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>, u16), EngineError> {
        let completion = self
            .await_poll_completion(
                connection,
                polling,
                initial_value,
                initial_response,
                credential_scope,
                metrics,
            )
            .await?;
        let active_key = completion.active_key.clone();
        let result = self
            .complete_poll(
                connection,
                polling,
                completion.value,
                completion.response,
                completion.job_id.as_ref(),
                credential_scope,
                metrics,
            )
            .await;
        if result.is_ok() {
            if let Some(key) = active_key {
                remove_active_job(&key);
            }
        }
        result
    }

    async fn resume_poll(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>, u16), EngineError> {
        let completion = self
            .await_resumed_poll_completion(connection, polling, credential_scope, metrics)
            .await?;
        let active_key = completion.active_key.clone();
        let result = self
            .complete_poll(
                connection,
                polling,
                completion.value,
                completion.response,
                completion.job_id.as_ref(),
                credential_scope,
                metrics,
            )
            .await;
        if result.is_ok() {
            if let Some(key) = active_key {
                remove_active_job(&key);
            }
        }
        result
    }

    async fn await_poll_completion(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        initial_value: Value,
        initial_response: ResponseData,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<PollCompletion, EngineError> {
        let job_id = polling_job_id(&initial_value, &initial_response, polling);
        match poll_state(&initial_value, polling)? {
            Some(PollState::Success) => {
                return Ok(PollCompletion {
                    value: initial_value,
                    response: initial_response,
                    job_id,
                    active_key: None,
                });
            }
            Some(PollState::Failure) => {
                return Err(EngineError::InvalidResponse(ErrorDetail::from(
                    "asynchronous operation reported a failure status",
                )));
            }
            Some(PollState::Pending) | None => {}
        }
        let poll_url = poll_url(&initial_value, &initial_response, polling)?;
        if !polling.allow_cross_origin && !same_origin(&initial_response.final_url, &poll_url) {
            return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                "cross-origin polling is blocked",
            )));
        }

        // Narrowed in place, so everything derived afterwards inherits the
        // restriction: the remote cancellation, the result URL, and — because
        // the caller shares this scope — any later pagination request as well.
        credential_scope.narrow(&poll_url);
        let active_key = self.register_polled_job(
            connection,
            polling,
            &poll_url,
            job_id.as_ref(),
            credential_scope,
        )?;
        self.await_poll_url(
            connection,
            polling,
            poll_url,
            job_id,
            active_key,
            credential_scope,
            metrics,
        )
        .await
    }

    async fn await_resumed_poll_completion(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<PollCompletion, EngineError> {
        let base_url = &credential_scope.origin.clone();
        let resume = polling.resume.as_ref().ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from("polling resume configuration is missing"))
        })?;
        validate_job_id(&resume.job_id)?;
        let job_id = Value::String(resume.job_id.clone());
        let template = polling.url_template.as_deref().ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from(
                "polling resume requires url_template and cannot infer a prior Location URL",
            ))
        })?;
        let target = render_poll_template_from_base(template, base_url, Some(&job_id))?;
        let poll_url = base_url.join(&target).map_err(|_| {
            EngineError::InvalidUrl(ErrorDetail::from("polling resume URL is invalid"))
        })?;
        if !polling.allow_cross_origin && !same_origin(base_url, &poll_url) {
            return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                "cross-origin polling is blocked",
            )));
        }
        credential_scope.narrow(&poll_url);
        let active_key = self.register_polled_job(
            connection,
            polling,
            &poll_url,
            Some(&job_id),
            credential_scope,
        )?;
        self.await_poll_url(
            connection,
            polling,
            poll_url,
            Some(job_id),
            active_key,
            credential_scope,
            metrics,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn await_poll_url(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        poll_url: Url,
        job_id: Option<Value>,
        active_key: String,
        credential_scope: &CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<PollCompletion, EngineError> {
        if polling.max_attempts == 0 {
            remove_active_job(&active_key);
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "polling max_attempts must be greater than zero",
            )));
        }
        let poll_started = Instant::now();
        let budget = polling.max_wait_ms.map(Duration::from_millis);
        // `max_wait_ms` is a hard bound: it caps the backoff sleep *and* the
        // request itself, so a slow poll cannot run past the configured limit.
        let remaining = |elapsed: Duration| budget.map(|budget| budget.saturating_sub(elapsed));
        let mut interval_ms = polling.interval_ms;
        let mut attempts = 0_u32;
        for _ in 0..polling.max_attempts {
            if remaining(poll_started.elapsed()).is_some_and(|left| left.is_zero()) {
                break;
            }
            if interval_ms > 0 {
                let delay = Duration::from_millis(interval_ms);
                let delay = remaining(poll_started.elapsed()).map_or(delay, |left| delay.min(left));
                sleep(delay).await;
            }
            let mut request = self.prepare_followup_request(
                connection,
                poll_url.clone(),
                polling.method.clone(),
                connection
                    .request
                    .timeout_ms
                    .unwrap_or(self.config.request_timeout_ms),
                CachePolicy::default(),
            );
            credential_scope.apply(&mut request, None);
            let attempt = self.execute_prepared_json(connection, request, metrics, true);
            // Counted before awaiting: once the request is issued it has been
            // sent, whether or not the remaining budget lets us read the reply.
            let (value, response) = match remaining(poll_started.elapsed()) {
                Some(left) if left.is_zero() => break,
                Some(left) => {
                    attempts = attempts.saturating_add(1);
                    match tokio::time::timeout(left, attempt).await {
                        Ok(result) => result?,
                        Err(_) => break,
                    }
                }
                None => {
                    attempts = attempts.saturating_add(1);
                    attempt.await?
                }
            };
            match poll_state(&value, polling)? {
                Some(PollState::Success) => {
                    return Ok(PollCompletion {
                        value,
                        response,
                        job_id,
                        active_key: Some(active_key),
                    });
                }
                Some(PollState::Failure) => {
                    remove_active_job(&active_key);
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "asynchronous operation reported a failure status",
                    )));
                }
                Some(PollState::Pending) => {}
                None => {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "poll response has no status at status_path",
                    )));
                }
            }
            interval_ms = ((interval_ms as f64 * polling.interval_backoff.max(1.0))
                .min(polling.max_interval_ms as f64)) as u64;
        }

        self.cancel_active_job(&active_key, RemoteCancelTrigger::PollTimeout)
            .await;
        // Report the attempts actually issued: `max_wait_ms` can end the loop
        // before `max_attempts` is reached.
        Err(EngineError::PollingTimeout { attempts })
    }

    fn register_polled_job(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        poll_url: &Url,
        job_id: Option<&Value>,
        credential_scope: &CredentialScope,
    ) -> Result<String, EngineError> {
        let key = poll_url.as_str().to_owned();
        let recovery = job_id
            .and_then(public_job_id)
            .map(|job_id| AsyncJobRecovery {
                contract: ASYNC_JOB_RECOVERY_CONTRACT.to_owned(),
                job_id,
                cancel_requested: false,
                cancel_accepted: None,
            });
        let cancel = polling
            .cancel
            .as_ref()
            .map(|cancel| {
                self.prepare_remote_cancel(
                    connection,
                    polling,
                    cancel,
                    poll_url,
                    job_id,
                    credential_scope,
                )
            })
            .transpose()?;
        register_active_job(key.clone(), ActiveAsyncJob { recovery, cancel });
        Ok(key)
    }

    fn prepare_remote_cancel(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        cancel: &PollingCancelConfig,
        poll_url: &Url,
        job_id: Option<&Value>,
        credential_scope: &CredentialScope,
    ) -> Result<ActiveRemoteCancel, EngineError> {
        if cancel.timeout_ms == 0 {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "polling cancel timeout_ms must be greater than zero",
            )));
        }
        let target = match cancel.url_template.as_deref() {
            Some(template) => {
                let rendered = render_poll_template_from_base(template, poll_url, job_id)?;
                poll_url.join(&rendered).map_err(|_| {
                    EngineError::InvalidUrl(ErrorDetail::from("polling cancel URL is invalid"))
                })?
            }
            None => poll_url.clone(),
        };
        if !polling.allow_cross_origin && !same_origin(poll_url, &target) {
            return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                "cross-origin polling cancellation is blocked",
            )));
        }
        let mut request = self.prepare_followup_request(
            connection,
            target,
            cancel.method.clone(),
            cancel.timeout_ms,
            CachePolicy::default(),
        );
        credential_scope.apply(&mut request, None);
        request.retry.max_attempts = 1;
        Ok(ActiveRemoteCancel {
            request,
            scope: credential_scope.clone(),
            on_cancellation: cancel.on_cancellation,
            on_deadline: cancel.on_deadline,
            on_poll_timeout: cancel.on_poll_timeout,
        })
    }

    fn prepare_followup_request(
        &self,
        connection: &ConnectionConfig,
        url: Url,
        method: HttpMethod,
        timeout_ms: u64,
        cache: CachePolicy,
    ) -> PreparedRequest {
        PreparedRequest {
            url,
            method,
            headers: connection.headers.clone(),
            auth: connection.auth.clone(),
            body: PreparedBody::None,
            timeout: Duration::from_millis(timeout_ms),
            allow_redirects: connection.request.allow_redirects,
            max_redirects: connection.request.max_redirects,
            retry: connection.retry.clone(),
            cookies: connection.cookies.clone(),
            admitted_jar: None,
            caller_session: connection.cookies.session.clone(),
            cache,
            circuit_breaker: connection.circuit_breaker.clone(),
            requests_per_second: connection.requests_per_second,
            tls: connection.tls.clone(),
            proxy: connection.proxy.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_poll(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        status_value: Value,
        status_response: ResponseData,
        job_id: Option<&Value>,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>, u16), EngineError> {
        if polling.result_url_path.is_none() && polling.result_url_template.is_none() {
            return Ok((
                poll_result(status_value, polling)?,
                status_response.final_url,
                status_response.attempts,
                status_response.headers,
                status_response.status,
            ));
        }
        let request = self.prepare_poll_result_request(
            connection,
            polling,
            &status_value,
            &status_response,
            job_id,
            credential_scope,
        )?;
        let (value, response) = self
            .execute_prepared_json(connection, request, metrics, true)
            .await?;
        Ok((
            poll_result(value, polling)?,
            response.final_url,
            response.attempts,
            response.headers,
            response.status,
        ))
    }

    fn prepare_poll_result_request(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        status_value: &Value,
        status_response: &ResponseData,
        job_id: Option<&Value>,
        credential_scope: &mut CredentialScope,
    ) -> Result<PreparedRequest, EngineError> {
        let target = polling_result_url(status_value, status_response, polling, job_id)?;
        if !polling.allow_cross_origin && !same_origin(&status_response.final_url, &target) {
            return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                "cross-origin polling result URL is blocked",
            )));
        }
        let mut request = PreparedRequest {
            url: target,
            method: polling.result_method.clone(),
            headers: connection.headers.clone(),
            auth: connection.auth.clone(),
            body: PreparedBody::None,
            timeout: Duration::from_millis(
                connection
                    .request
                    .timeout_ms
                    .unwrap_or(self.config.request_timeout_ms),
            ),
            allow_redirects: connection.request.allow_redirects,
            max_redirects: connection.request.max_redirects,
            retry: connection.retry.clone(),
            cookies: connection.cookies.clone(),
            admitted_jar: None,
            caller_session: connection.cookies.session.clone(),
            cache: connection.cache.clone(),
            circuit_breaker: connection.circuit_breaker.clone(),
            requests_per_second: connection.requests_per_second,
            tls: connection.tls.clone(),
            proxy: connection.proxy.clone(),
        };
        credential_scope.narrow(&request.url);
        credential_scope.apply(&mut request, None);
        Ok(request)
    }

    async fn resolve_upload_source(
        &self,
        file: &FileTransferInput,
    ) -> Result<PathBuf, EngineError> {
        let root = self.transfer_root().await?;
        let requested = required_path(&file.path)?;
        let candidate = if requested.is_absolute() {
            requested
        } else {
            root.join(requested)
        };
        let source = fs::canonicalize(candidate).await.map_err(file_io)?;
        ensure_within_root(&source, &root)?;
        Ok(source)
    }

    async fn resolve_download_target(
        &self,
        file: &FileTransferInput,
    ) -> Result<PathBuf, EngineError> {
        let root = self.transfer_root().await?;
        let requested = required_path(&file.path)?;
        let candidate = if requested.is_absolute() {
            requested
        } else {
            root.join(requested)
        };
        let filename = candidate.file_name().ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from("download path must include a filename"))
        })?;
        let parent = candidate.parent().ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from("download path has no parent directory"))
        })?;
        let parent = fs::canonicalize(parent).await.map_err(file_io)?;
        ensure_within_root(&parent, &root)?;
        let target = parent.join(filename);
        if fs::try_exists(&target).await.map_err(file_io)? {
            let metadata = fs::symlink_metadata(&target).await.map_err(file_io)?;
            if metadata.is_dir() {
                return Err(EngineError::FileIo(ErrorDetail::from(
                    "download destination is a directory",
                )));
            }
            if !file.overwrite {
                return Err(EngineError::FileIo(ErrorDetail::from(
                    "download destination already exists",
                )));
            }
        }
        Ok(target)
    }

    fn ensure_file_transfers_allowed(&self) -> Result<(), EngineError> {
        if self.config.allow_file_transfers {
            Ok(())
        } else {
            Err(EngineError::PolicyViolation(ErrorDetail::from(
                "file transfers are not enabled for this engine",
            )))
        }
    }

    /// Confinement root for every local file transfer.
    ///
    /// `allow_file_transfers` alone is not sufficient: without a `file_root`
    /// there is no boundary to enforce, so the engine would accept any absolute
    /// path and resolve relative paths against the process working directory.
    /// The documented contract requires both, and this is where that is enforced.
    async fn transfer_root(&self) -> Result<PathBuf, EngineError> {
        self.ensure_file_transfers_allowed()?;
        self.canonical_file_root().await?.ok_or_else(|| {
            EngineError::PolicyViolation(ErrorDetail::from(
                "file transfers require a configured file_root",
            ))
        })
    }

    async fn canonical_file_root(&self) -> Result<Option<PathBuf>, EngineError> {
        let Some(root) = self.config.file_root.as_deref() else {
            return Ok(None);
        };
        let root = required_path(root)?;
        let root = if root.is_absolute() {
            root
        } else {
            canonical_current_directory().await?.join(root)
        };
        let root = fs::canonicalize(root).await.map_err(file_io)?;
        if !fs::metadata(&root).await.map_err(file_io)?.is_dir() {
            return Err(EngineError::FileIo(ErrorDetail::from(
                "configured file_root is not a directory",
            )));
        }
        Ok(Some(root))
    }

    fn prepare_request(
        &self,
        connection: &ConnectionConfig,
        parameters: &JsonObject,
        url_override: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Result<PreparedRequest, EngineError> {
        let template = url_override.unwrap_or(&connection.url);
        if template.trim().is_empty() {
            return Err(EngineError::InvalidUrl(ErrorDetail::from("URL is empty")));
        }
        let (rendered_url, consumed) = render_template(template, parameters, true);
        let mut url = Url::parse(&rendered_url)
            .map_err(|_| EngineError::InvalidUrl(ErrorDetail::from("rendered URL is not valid")))?;
        let legacy_query_only = matches!(
            &connection.method,
            HttpMethod::Get | HttpMethod::Head | HttpMethod::Delete | HttpMethod::Options
        ) || connection.request.body_type == BodyType::None;
        let mut query_parameters = JsonObject::new();
        let mut body_parameters = JsonObject::new();
        let mut headers = connection.headers.clone();
        let mut cookies = Vec::new();
        for (name, value) in parameters {
            let spec = connection.parameters.iter().find(|spec| spec.name == *name);
            let location = spec.map_or(ParameterLocation::Auto, |spec| spec.location);
            if consumed.contains(name) {
                if !matches!(location, ParameterLocation::Auto | ParameterLocation::Path) {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "parameter is used in the URL but has a non-path location",
                    )));
                }
                refuse_null_in_text(value)?;
                continue;
            }
            match location {
                ParameterLocation::Path => {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "path parameter has no matching URL placeholder",
                    )));
                }
                ParameterLocation::Query => {
                    refuse_null_in_text(value)?;
                    query_parameters.insert(name.clone(), value.clone());
                }
                ParameterLocation::Header => {
                    refuse_null_in_text(value)?;
                    insert_header(&mut headers, name, parameter_header_value(value));
                }
                ParameterLocation::Body => {
                    body_parameters.insert(name.clone(), value.clone());
                }
                ParameterLocation::Cookie => {
                    refuse_null_in_text(value)?;
                    validate_cookie_name(name)?;
                    cookies.push((name.clone(), cookie_value(value)?));
                }
                ParameterLocation::Auto if legacy_query_only => {
                    refuse_null_in_text(value)?;
                    query_parameters.insert(name.clone(), value.clone());
                }
                ParameterLocation::Auto => {
                    body_parameters.insert(name.clone(), value.clone());
                }
            }
        }
        if let Some(key) = idempotency_key {
            apply_idempotency(
                connection,
                key,
                &mut query_parameters,
                &mut body_parameters,
                &mut headers,
            )?;
        }
        append_query(&mut url, &query_parameters, &connection.parameters)?;
        append_cookies(&mut headers, &cookies);

        let build_body = !body_parameters.is_empty()
            || !legacy_query_only
            || (connection.request.body_type == BodyType::Raw
                && connection.request.raw_body.is_some());
        let body = if !build_body {
            PreparedBody::None
        } else {
            match connection.request.body_type {
                BodyType::Json => PreparedBody::Json(Value::Object(body_parameters)),
                BodyType::FormUrlencoded => {
                    for value in body_parameters.values() {
                        refuse_null_in_text(value)?;
                    }
                    PreparedBody::Form(
                        body_parameters
                            .iter()
                            .map(|(key, value)| (key.clone(), value_as_text(value)))
                            .collect(),
                    )
                }
                BodyType::Multipart => {
                    for value in body_parameters.values() {
                        refuse_null_in_text(value)?;
                    }
                    multipart_body(&body_parameters, self.config.max_request_bytes)?
                }
                BodyType::Raw => {
                    let raw = connection.request.raw_body.as_deref().unwrap_or_default();
                    let (rendered, consumed) = render_template(raw, parameters, false);
                    for name in &consumed {
                        if let Some(value) = parameters.get(name) {
                            refuse_null_in_text(value)?;
                        }
                    }
                    ensure_request_size(rendered.len(), self.config.max_request_bytes)?;
                    PreparedBody::Raw(rendered)
                }
                BodyType::None => PreparedBody::None,
            }
        };
        match &body {
            PreparedBody::Json(value) => {
                let size = serde_json::to_vec(value)
                    .map_err(|_| {
                        EngineError::InvalidInput(ErrorDetail::from(
                            "JSON body could not be serialized",
                        ))
                    })?
                    .len();
                ensure_request_size(size, self.config.max_request_bytes)?;
            }
            PreparedBody::Form(values) => {
                let mut serializer = url::form_urlencoded::Serializer::new(String::new());
                for (name, value) in values {
                    serializer.append_pair(name, value);
                }
                ensure_request_size(serializer.finish().len(), self.config.max_request_bytes)?;
            }
            PreparedBody::None
            | PreparedBody::Raw(_)
            | PreparedBody::Multipart { .. }
            | PreparedBody::Stream(_) => {}
        }

        let mut retry = connection.retry.clone();
        if idempotency_key.is_some() {
            retry.retry_non_idempotent = true;
        }
        Ok(PreparedRequest {
            url,
            method: connection.method.clone(),
            headers,
            auth: connection.auth.clone(),
            body,
            timeout: Duration::from_millis(
                connection
                    .request
                    .timeout_ms
                    .unwrap_or(self.config.request_timeout_ms),
            ),
            allow_redirects: connection.request.allow_redirects,
            max_redirects: connection.request.max_redirects,
            retry,
            cookies: connection.cookies.clone(),
            admitted_jar: None,
            caller_session: connection.cookies.session.clone(),
            cache: connection.cache.clone(),
            circuit_breaker: connection.circuit_breaker.clone(),
            requests_per_second: connection.requests_per_second,
            tls: connection.tls.clone(),
            proxy: connection.proxy.clone(),
        })
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

fn required_file_input(request: &ExecutionRequest) -> Result<&FileTransferInput, EngineError> {
    request.input.file.as_ref().ok_or_else(|| {
        EngineError::InvalidInput(ErrorDetail::from("operation requires input.file"))
    })
}

fn required_path(value: &str) -> Result<PathBuf, EngineError> {
    if value.trim().is_empty() {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "file path cannot be empty",
        )))
    } else {
        Ok(PathBuf::from(value))
    }
}

fn transfer_limit(config: &EngineConfig, file: &FileTransferInput) -> Result<u64, EngineError> {
    if config.max_file_transfer_bytes == 0 {
        return Err(EngineError::PolicyViolation(ErrorDetail::from(
            "max_file_transfer_bytes must be greater than zero",
        )));
    }
    match file.max_bytes {
        Some(0) => Err(EngineError::InvalidInput(ErrorDetail::from(
            "input.file.max_bytes must be greater than zero",
        ))),
        Some(limit) => Ok(limit.min(config.max_file_transfer_bytes)),
        None => Ok(config.max_file_transfer_bytes),
    }
}

fn validated_checksum(value: Option<&str>) -> Result<Option<String>, EngineError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "expected_sha256 must contain exactly 64 hexadecimal characters",
        )));
    }
    Ok(Some(value.to_ascii_lowercase()))
}

fn ensure_within_root(path: &Path, root: &Path) -> Result<(), EngineError> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(EngineError::PolicyViolation(ErrorDetail::from(
            "file path is outside the configured file_root",
        )))
    }
}

async fn canonical_current_directory() -> Result<PathBuf, EngineError> {
    let current = std::env::current_dir().map_err(file_io)?;
    fs::canonicalize(current).await.map_err(file_io)
}

async fn hash_file(path: &Path, limit: u64) -> Result<String, EngineError> {
    let mut file = fs::File::open(path).await.map_err(file_io)?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(file_io)?;
        if read == 0 {
            break;
        }
        let chunk = buffer.get(..read).ok_or_else(|| {
            EngineError::Runtime(ErrorDetail::from(
                "file read reported more bytes than its buffer",
            ))
        })?;
        let read = u64::try_from(read).map_err(|_| {
            EngineError::Runtime(ErrorDetail::from("file read length overflowed u64"))
        })?;
        if total.saturating_add(read) > limit {
            return Err(EngineError::FileTooLarge { limit_bytes: limit });
        }
        digest.update(chunk);
        total = total.saturating_add(read);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_multipart_name(value: &str) -> Result<(), EngineError> {
    if value.trim().is_empty() || value.contains(['\r', '\n', '\0']) {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "upload multipart name is empty or contains unsupported characters",
        )))
    } else {
        Ok(())
    }
}

fn file_io(error: std::io::Error) -> EngineError {
    EngineError::FileIo(crate::error::io_detail(&error))
}

enum PollState {
    Pending,
    Success,
    /// The remote status is not kept: it is third-party text.
    Failure,
}

fn poll_state(response: &Value, polling: &PollingConfig) -> Result<Option<PollState>, EngineError> {
    let Some(value) = json_path::get(response, &polling.status_path) else {
        return Ok(None);
    };
    // A null status is not the empty string: it matches no configured value,
    // even an empty one, and is reported instead of being guessed.
    if value.is_null() {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "asynchronous status is null",
        )));
    }
    let status = value_as_text(value);
    if polling
        .pending_values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(&status))
    {
        return Ok(Some(PollState::Pending));
    }
    if polling
        .success_values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(&status))
    {
        return Ok(Some(PollState::Success));
    }
    if polling
        .failure_values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(&status))
    {
        return Ok(Some(PollState::Failure));
    }
    Err(EngineError::InvalidResponse(ErrorDetail::from(
        "unknown asynchronous status",
    )))
}

fn evaluate_application_success(
    response: &Value,
    connection: &ConnectionConfig,
) -> Result<(), EngineError> {
    if let Some(path) = &connection.response.error_path {
        if json_path::get(response, path)
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
        {
            // The remote message at error_path is not captured: it is
            // third-party text, and the error contract carries none.
            return Err(EngineError::Application(ErrorDetail::from(
                "the response reports an error at error_path",
            )));
        }
    }
    if let Some(condition) = &connection.response.success_when {
        if !matches_success_condition(response, condition) {
            return Err(EngineError::Application(ErrorDetail::from(
                "success_when condition was not satisfied",
            )));
        }
    }
    Ok(())
}

fn matches_success_condition(response: &Value, condition: &Value) -> bool {
    static NULL_VALUE: Value = Value::Null;
    if let Some(conditions) = condition.as_array() {
        return conditions
            .iter()
            .all(|condition| matches_success_condition(response, condition));
    }
    let Some(condition) = condition.as_object() else {
        return condition.as_bool().unwrap_or(!condition.is_null());
    };
    let path = condition.get("path").and_then(Value::as_str);
    let value = path
        .map(|path| json_path::get(response, path).unwrap_or(&NULL_VALUE))
        .unwrap_or(response);
    if let Some(expected) = condition.get("exists").and_then(Value::as_bool) {
        let exists = condition
            .get("path")
            .and_then(Value::as_str)
            .and_then(|path| json_path::get(response, path))
            .is_some();
        return exists == expected;
    }
    if let Some(expected) = condition.get("truthy").and_then(Value::as_bool) {
        return json_truthy(value) == expected;
    }
    if let Some(expected) = condition.get("equals") {
        return value == expected;
    }
    if let Some(expected) = condition.get("not_equals") {
        return value != expected;
    }
    if let Some(values) = condition.get("in").and_then(Value::as_array) {
        return values.contains(value);
    }
    if let Some(values) = condition.get("not_in").and_then(Value::as_array) {
        return !values.contains(value);
    }
    json_truthy(value)
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn poll_result(response: Value, polling: &PollingConfig) -> Result<Value, EngineError> {
    match &polling.result_path {
        Some(path) => json_path::get(&response, path).cloned().ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from("polling result path was not found"))
        }),
        None => Ok(response),
    }
}

fn poll_url(
    response: &Value,
    response_data: &ResponseData,
    polling: &PollingConfig,
) -> Result<Url, EngineError> {
    let target = if let Some(path) = &polling.url_path {
        json_path::get(response, path)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                EngineError::InvalidResponse(ErrorDetail::from("polling URL path was not found"))
            })?
            .to_owned()
    } else if let Some(template) = &polling.url_template {
        let job_id = polling_job_id(response, response_data, polling).ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from("polling job id was not found"))
        })?;
        render_poll_template(template, response_data, Some(&job_id))?
    } else if let Some(header) = &polling.location_header {
        response_data
            .headers
            .get(&header.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| {
                EngineError::InvalidResponse(ErrorDetail::from(
                    "polling response has no location header",
                ))
            })?
    } else {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling requires url_path, url_template, or location_header",
        )));
    };

    response_data
        .final_url
        .join(&target)
        .map_err(|_| EngineError::InvalidUrl(ErrorDetail::from("polling URL is invalid")))
}

fn polling_job_id(
    response: &Value,
    response_data: &ResponseData,
    polling: &PollingConfig,
) -> Option<Value> {
    if let Some(header) = &polling.id_header {
        response_data
            .headers
            .get(&header.to_ascii_lowercase())
            .cloned()
            .map(Value::String)
    } else {
        // A null id is no id: rendered into a URL it would become "".
        json_path::get(response, &polling.id_path)
            .filter(|value| !value.is_null())
            .cloned()
    }
}

fn polling_result_url(
    response: &Value,
    response_data: &ResponseData,
    polling: &PollingConfig,
    job_id: Option<&Value>,
) -> Result<Url, EngineError> {
    let target = if let Some(template) = &polling.result_url_template {
        render_poll_template(template, response_data, job_id)?
    } else {
        let path = polling.result_url_path.as_deref().ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from("polling result URL is not configured"))
        })?;
        let template = json_path::get(response, path)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                EngineError::InvalidResponse(ErrorDetail::from(
                    "polling result URL path was not found",
                ))
            })?;
        render_poll_template(template, response_data, job_id)?
    };
    response_data
        .final_url
        .join(&target)
        .map_err(|_| EngineError::InvalidUrl(ErrorDetail::from("polling result URL is invalid")))
}

fn render_poll_template(
    template: &str,
    response_data: &ResponseData,
    job_id: Option<&Value>,
) -> Result<String, EngineError> {
    render_poll_template_from_base(template, &response_data.final_url, job_id)
}

fn render_poll_template_from_base(
    template: &str,
    base_url: &Url,
    job_id: Option<&Value>,
) -> Result<String, EngineError> {
    let base = base_url.origin().ascii_serialization();
    let template = template.replace("{base}", &base);
    if !template.contains("{id}") && !template.contains("{job_id}") {
        return Ok(template);
    }
    let job_id = job_id.ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from("polling result URL requires a job id"))
    })?;
    let parameters = Map::from_iter([
        ("id".to_owned(), job_id.clone()),
        ("job_id".to_owned(), job_id.clone()),
    ]);
    Ok(render_template(&template, &parameters, true).0)
}

fn public_job_id(value: &Value) -> Option<String> {
    let value = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    validate_job_id(&value).ok().map(|_| value)
}

fn validate_job_id(value: &str) -> Result<(), EngineError> {
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(|character| character.is_control())
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling job_id must contain 1 to 512 non-control characters",
        )));
    }
    Ok(())
}

fn resolve_parameters(
    connection: &ConnectionConfig,
    source: &JsonObject,
) -> Result<JsonObject, EngineError> {
    let mut parameters = connection.static_parameters.clone();
    parameters.extend(source.clone());
    let source_value = Value::Object(source.clone());

    for parameter in &connection.parameters {
        let value = match parameter.mode {
            // A fixed parameter without a value has nothing to send: dropping
            // it would make a configuration mistake indistinguishable from an
            // optional parameter that is legitimately absent.
            ParameterMode::Fixed if parameter.value.is_none() => {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "fixed parameter has no value",
                )));
            }
            ParameterMode::Fixed => parameter.value.clone(),
            ParameterMode::Mapped => {
                let source_path = parameter.source.as_deref().unwrap_or(&parameter.name);
                source
                    .get(source_path)
                    .or_else(|| json_path::get(&source_value, source_path))
                    .cloned()
                    .or_else(|| parameter.value.clone())
            }
        };
        match value {
            Some(value) => {
                parameters.insert(parameter.name.clone(), value);
            }
            None if parameter.required => {
                return Err(EngineError::MissingParameter(ErrorDetail::from(
                    "a required parameter has no value",
                )));
            }
            None => {}
        }
    }
    Ok(parameters)
}

fn response_records(
    connection: &ConnectionConfig,
    response: &Value,
) -> Result<Vec<Value>, EngineError> {
    let selected = match &connection.response.records_path {
        Some(path) => json_path::get(response, path).ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from("records path was not found"))
        })?,
        None => response,
    };
    if connection.response.iterate_on.is_empty() {
        return match selected {
            Value::Array(values) => Ok(values.clone()),
            Value::Object(_) => Ok(vec![selected.clone()]),
            Value::Null => Ok(Vec::new()),
            _ => Err(EngineError::InvalidResponse(ErrorDetail::from(
                "records value must be an array, object, or null",
            ))),
        };
    }
    let mut output = Vec::new();
    expand_iterations(
        selected,
        &connection.response.iterate_on,
        0,
        &Map::new(),
        &mut output,
    );
    Ok(output)
}

fn batch_payload(batch: &BatchConfig, parameters: Vec<JsonObject>) -> JsonObject {
    let values = match batch.input_format {
        // Every record was checked by `flat_batch_item`, so each one contributes
        // exactly one element and the array stays aligned with the input.
        BatchInputFormat::FlatArray => Value::Array(
            parameters
                .into_iter()
                .map(|parameters| parameters.into_values().next().unwrap_or(Value::Null))
                .collect(),
        ),
        BatchInputFormat::Array | BatchInputFormat::Object => {
            Value::Array(parameters.into_iter().map(Value::Object).collect())
        }
    };
    Map::from_iter([(batch.input_key.clone(), values)])
}

/// The single value a record contributes to a `flat_array` batch.
///
/// A flat array has one element per record and nothing else to align results
/// with. A record that resolves to several parameters, or to a `null`, used to
/// contribute its first non-null value in key order, or nothing at all, which
/// silently picked a field or shifted every later record. Both are refused.
fn flat_batch_item(parameters: &JsonObject) -> Result<&Value, EngineError> {
    let mut values = parameters.values();
    match (values.next(), values.next()) {
        (Some(value), None) if !value.is_null() => Ok(value),
        _ => Err(EngineError::InvalidInput(ErrorDetail::from(
            "a flat_array batch record must resolve to exactly one non-null parameter",
        ))),
    }
}

fn batch_response(
    response: &Value,
    batch: &BatchConfig,
    config: &ResponseConfig,
) -> Result<Vec<JsonObject>, EngineError> {
    let selected = if batch.output_path.is_empty() {
        response
    } else {
        json_path::get(response, &batch.output_path).ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from("batch output path was not found"))
        })?
    };
    let values = selected.as_array().ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from("batch output must be an array"))
    })?;
    values
        .iter()
        .map(|value| {
            if value.is_null() {
                return Ok(JsonObject::new());
            }
            let mut row: JsonObject = if config.output_mapping.is_empty() {
                value
                    .as_object()
                    .cloned()
                    .unwrap_or_else(|| Map::from_iter([("value".to_owned(), value.clone())]))
            } else {
                config
                    .output_mapping
                    .iter()
                    .map(|mapping| {
                        let path = mapping.path.strip_prefix("[0].").unwrap_or(&mapping.path);
                        let value = json_path::get(value, path)
                            .cloned()
                            .or_else(|| mapping.default.clone())
                            .unwrap_or(Value::Null);
                        (mapping.column.clone(), value)
                    })
                    .collect()
            };
            apply_transforms(&mut row, &config.transforms)?;
            Ok(row)
        })
        .collect()
}

fn expand_iterations(
    current: &Value,
    iterations: &[crate::IterationSpec],
    level: usize,
    context: &JsonObject,
    output: &mut Vec<Value>,
) {
    let Some(iteration) = iterations.get(level) else {
        output.push(Value::Object(context.clone()));
        return;
    };
    let selected = if iteration.path.is_empty() {
        Some(current)
    } else {
        json_path::get(current, &iteration.path)
    };
    let Some(selected) = selected else { return };
    match selected {
        Value::Array(values) => {
            for value in values {
                let mut next = context.clone();
                next.insert(iteration.alias.clone(), value.clone());
                expand_iterations(value, iterations, level + 1, &next, output);
            }
        }
        Value::Null => {}
        value => {
            let mut next = context.clone();
            next.insert(iteration.alias.clone(), value.clone());
            expand_iterations(value, iterations, level + 1, &next, output);
        }
    }
}

fn map_records(
    values: Vec<Value>,
    response: &ResponseConfig,
) -> Result<Vec<JsonObject>, EngineError> {
    values
        .iter()
        .map(|value| {
            let mut row = map_value(value, &response.output_mapping);
            apply_transforms(&mut row, &response.transforms)?;
            Ok(row)
        })
        .collect()
}

fn map_value(value: &Value, mappings: &[OutputMapping]) -> JsonObject {
    if mappings.is_empty() {
        return match value {
            Value::Object(object) => object.clone(),
            value => Map::from_iter([("value".to_owned(), value.clone())]),
        };
    }

    mappings
        .iter()
        .map(|mapping| {
            let value = json_path::get(value, &mapping.path)
                .cloned()
                .or_else(|| mapping.default.clone())
                .unwrap_or(Value::Null);
            (mapping.column.clone(), value)
        })
        .collect()
}

fn apply_transforms(
    row: &mut JsonObject,
    transforms: &[ResponseTransform],
) -> Result<(), EngineError> {
    for transform in transforms {
        if let Some(condition) = transform.condition.as_deref() {
            if !transform_condition(row, condition) {
                continue;
            }
        }
        let source = row.get(&transform.source).cloned().unwrap_or(Value::Null);
        let value = transform_value(&source, transform).map_err(|failure| {
            EngineError::InvalidResponse(ErrorDetail::from(failure.describe()))
        })?;
        row.insert(transform.column.clone(), value);
    }
    Ok(())
}

/// A transform condition, `column == 'literal'` or `column != 'literal'`.
struct TransformCondition<'a> {
    column: &'a str,
    expected: &'a str,
    equals: bool,
}

fn parse_transform_condition(condition: &str) -> Option<TransformCondition<'_>> {
    let is_operator_or_quote = |character: char| matches!(character, '=' | '!' | '\'' | '"');
    // The column cannot contain `=`, `!` or a quote, so the first of those
    // characters starts the operator. Searching the whole string for `==`
    // instead would find one inside a quoted literal, as in
    // `status != 'a==b'`, and split there.
    let operator = condition.find(is_operator_or_quote)?;
    let (left, rest) = condition.split_at(operator);
    let (right, equals) = if let Some(right) = rest.strip_prefix("==") {
        (right, true)
    } else {
        (rest.strip_prefix("!=")?, false)
    };
    let column = left.trim();
    if column.is_empty() {
        return None;
    }
    // The literal is either quoted with one matching pair, or bare. Trimming
    // quote characters from both ends would accept `'active` or `active"` and
    // compare against a value the author never wrote.
    let right = right.trim();
    let expected = match right.chars().next() {
        Some(quote @ ('\'' | '"')) => {
            let inner = right
                .strip_prefix(quote)
                .and_then(|rest| rest.strip_suffix(quote))?;
            if inner.contains(quote) {
                return None;
            }
            inner
        }
        _ if right.is_empty() || right.contains(is_operator_or_quote) => return None,
        _ => right,
    };
    Some(TransformCondition {
        column,
        expected,
        equals,
    })
}

/// Evaluates a condition validated by [`validate_transforms`].
///
/// A missing or `null` column satisfies neither `==` nor `!=`: comparing it
/// as an empty string would make `column == ''` match a value that is not
/// there. As in SQL, the comparison is unknown and the transform is skipped.
fn transform_condition(row: &JsonObject, condition: &str) -> bool {
    let Some(condition) = parse_transform_condition(condition) else {
        // Unreachable after validation; never apply a transform on a condition
        // that was not understood.
        return false;
    };
    match row.get(condition.column) {
        None | Some(Value::Null) => false,
        Some(actual) => (value_as_text(actual) == condition.expected) == condition.equals,
    }
}

/// Largest decimal count accepted by `round`: beyond it `f64` has no digits
/// left to round.
const MAX_ROUND_DECIMALS: u64 = 15;

/// Validates response transforms before any network activity.
///
/// An unknown operation, a missing or ill-typed argument, or a condition that
/// cannot be parsed used to leave the column untouched or write `null`, which
/// is indistinguishable from data. They are configuration errors.
fn validate_transforms(response: &ResponseConfig) -> Result<(), EngineError> {
    for transform in &response.transforms {
        let invalid =
            |reason: &'static str| Err(EngineError::InvalidInput(ErrorDetail::from(reason)));
        if transform.column.is_empty() {
            return invalid("has an empty column");
        }
        if transform.source.is_empty() {
            return invalid("has an empty source");
        }
        if transform
            .condition
            .as_deref()
            .is_some_and(|condition| parse_transform_condition(condition).is_none())
        {
            return invalid(
                "has a condition that is not `column == 'value'` or `column != 'value'`",
            );
        }
        let value = transform.value.as_ref();
        match transform.operation.as_str() {
            "add" | "subtract" | "multiply" | "divide" => {
                let Some(argument) = value else {
                    return invalid("requires a numeric value");
                };
                match numeric_operand(argument) {
                    Ok(argument) => {
                        if transform.operation == "divide" && argument.is_zero() {
                            return invalid("divides by zero");
                        }
                    }
                    Err(_) => return invalid("requires a numeric value"),
                }
            }
            "round" => {
                if let Some(decimals) = value {
                    if !decimals
                        .as_u64()
                        .is_some_and(|decimals| decimals <= MAX_ROUND_DECIMALS)
                    {
                        return invalid("requires an integer number of decimals from 0 to 15");
                    }
                }
            }
            "kelvin_to_celsius" | "celsius_to_kelvin" | "uppercase" | "lowercase" => {
                if value.is_some() {
                    return invalid("does not take a value");
                }
            }
            "prefix" | "suffix" => {
                if !value.is_some_and(is_text_scalar) {
                    return invalid("requires a string, number, or boolean value");
                }
            }
            "replace" => {
                let pair = value.and_then(Value::as_object).and_then(|pair| {
                    let find = pair.get("find")?.as_str()?;
                    pair.get("replace")?.as_str()?;
                    (pair.len() == 2 && !find.is_empty()).then_some(())
                });
                if pair.is_none() {
                    return invalid(
                        "requires a value with exactly a non-empty string 'find' and a string 'replace'",
                    );
                }
            }
            "default_if_null" => {
                if value.is_none() {
                    return invalid("requires a value");
                }
            }
            _ => {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "transform for column has an unknown operation",
                )));
            }
        }
    }
    Ok(())
}

fn is_text_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// Why a transform could not produce a value for one row.
#[derive(Debug, PartialEq, Eq)]
enum TransformFailure {
    /// The source is not a number, nor a string holding one.
    NotNumeric,
    /// The source is not text-like (a string, number, or boolean).
    NotText,
    /// The source is not a string.
    NotString,
    /// The exact result, or an operand, has no exact representation.
    Unrepresentable,
}

impl TransformFailure {
    fn describe(&self) -> &'static str {
        match self {
            Self::NotNumeric => "received a value that is not numeric",
            Self::NotText => "received a value that is not a string, number, or boolean",
            Self::NotString => "received a value that is not a string",
            Self::Unrepresentable => "produced a result that cannot be represented exactly",
        }
    }
}

/// Operand of a numeric transform.
///
/// Integers stay integers: routing every value through `f64` silently rewrites
/// identifiers and counters beyond 2^53, which are common in REST payloads.
/// `Unsigned` exists because JSON integers above `i64::MAX` are legal and
/// `serde_json` represents them exactly.
#[derive(Clone, Copy)]
enum Numeric {
    Signed(i64),
    Unsigned(u64),
    /// An integer written as a string and too large for `i64` or `u64`.
    /// Carrying it exactly keeps arithmetic that brings it back into range from
    /// being rounded on the way in.
    Wide(i128),
    Float(f64),
}

/// Largest integer magnitude that `f64` represents exactly, 2^53.
const F64_EXACT_INTEGER: i128 = 1 << 53;

impl Numeric {
    /// The value as `f64`, only when the conversion is exact.
    ///
    /// An integer beyond 2^53 has no exact `f64`; converting it would make
    /// floating point arithmetic start from a different number than the one
    /// received, so mixed or fractional arithmetic on it is refused.
    fn exact_f64(self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(value),
            integer => integer
                .as_i128()
                .filter(|value| value.unsigned_abs() <= F64_EXACT_INTEGER.unsigned_abs())
                .map(|value| value as f64),
        }
    }

    fn is_zero(self) -> bool {
        match self {
            Self::Float(value) => value == 0.0,
            integer => integer.as_i128() == Some(0),
        }
    }

    /// The exact integer value, when there is one.
    fn as_i128(self) -> Option<i128> {
        match self {
            Self::Signed(value) => Some(i128::from(value)),
            Self::Unsigned(value) => Some(i128::from(value)),
            Self::Wide(value) => Some(value),
            Self::Float(_) => None,
        }
    }

    fn is_integer(self) -> bool {
        self.as_i128().is_some()
    }

    fn into_value(self) -> Result<Value, TransformFailure> {
        match self {
            Self::Signed(value) => Ok(Value::Number(Number::from(value))),
            Self::Unsigned(value) => Ok(Value::Number(Number::from(value))),
            Self::Wide(value) => integer_value(value),
            Self::Float(value) => float_value(value),
        }
    }
}

/// A finite `f64` as a JSON number; NaN and infinities have no JSON spelling.
fn float_value(value: f64) -> Result<Value, TransformFailure> {
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or(TransformFailure::Unrepresentable)
}

/// Narrows an exact integer result back to a JSON representable integer.
///
/// Fails when the value fits neither `i64` nor `u64`: emitting a rounded `f64`
/// there would reintroduce exactly the silent precision loss this type exists
/// to prevent.
fn integer_value(value: i128) -> Result<Value, TransformFailure> {
    if let Ok(value) = i64::try_from(value) {
        return Ok(Value::Number(Number::from(value)));
    }
    u64::try_from(value)
        .map(|value| Value::Number(Number::from(value)))
        .map_err(|_| TransformFailure::Unrepresentable)
}

fn numeric_operand(value: &Value) -> Result<Numeric, TransformFailure> {
    if let Some(integer) = value.as_i64() {
        return Ok(Numeric::Signed(integer));
    }
    if let Some(integer) = value.as_u64() {
        return Ok(Numeric::Unsigned(integer));
    }
    if let Some(float) = value.as_f64() {
        return Ok(Numeric::Float(float));
    }
    let text = value.as_str().ok_or(TransformFailure::NotNumeric)?.trim();
    if let Ok(integer) = text.parse::<i64>() {
        return Ok(Numeric::Signed(integer));
    }
    if let Ok(integer) = text.parse::<u64>() {
        return Ok(Numeric::Unsigned(integer));
    }
    // A numeric identifier carried as a string can exceed `u64`. Parsing it as
    // `i128` before reaching for `f64` keeps the value exact, which matters
    // because the very next step may bring it back into a representable range.
    if let Ok(integer) = text.parse::<i128>() {
        return Ok(Numeric::Wide(integer));
    }
    // An integer literal too wide even for `i128` must not fall through to the
    // float parser, which would accept it rounded.
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TransformFailure::Unrepresentable);
    }
    match text.parse::<f64>() {
        Ok(float) if float.is_finite() => Ok(Numeric::Float(float)),
        Ok(_) => Err(TransformFailure::Unrepresentable),
        Err(_) => Err(TransformFailure::NotNumeric),
    }
}

/// Result of an exact integer computation on two integral operands.
enum IntegerOutcome {
    Exact(i128),
    /// The operands are integers but the result is not, as in `7 / 2`. Falling
    /// back to floating point is the answer the caller wants, provided both
    /// operands convert to `f64` exactly.
    Fractional,
    /// The result is an integer too large to represent. Falling back to
    /// floating point here would silently return a rounded value, which is the
    /// precision loss this type exists to prevent.
    Unrepresentable,
}

/// Applies `integer` when both operands are integral, and floating point
/// arithmetic when either operand is a float or the exact result is fractional.
///
/// `i128` is wide enough for any sum, difference, or product of two JSON
/// integers except the extremes of `u64 * u64`, which `checked_*` reports.
/// Floating point starts only from operands that convert to `f64` exactly, and
/// a non-finite result is refused: either would return a number other than the
/// one the arithmetic defines, without saying so.
fn numeric_arithmetic(
    left: Numeric,
    right: Numeric,
    integer: impl Fn(i128, i128) -> IntegerOutcome,
    float: impl Fn(f64, f64) -> f64,
) -> Result<Value, TransformFailure> {
    if let (Some(left), Some(right)) = (left.as_i128(), right.as_i128()) {
        match integer(left, right) {
            IntegerOutcome::Exact(result) => return integer_value(result),
            IntegerOutcome::Unrepresentable => return Err(TransformFailure::Unrepresentable),
            IntegerOutcome::Fractional => {}
        }
    }
    let (Some(left), Some(right)) = (left.exact_f64(), right.exact_f64()) else {
        return Err(TransformFailure::Unrepresentable);
    };
    float_value(float(left, right))
}

/// Wraps a checked `i128` operation: `None` means the exact result overflows.
fn checked_integer(result: Option<i128>) -> IntegerOutcome {
    result.map_or(IntegerOutcome::Unrepresentable, IntegerOutcome::Exact)
}

/// Applies one transform validated by [`validate_transforms`] to one value.
///
/// `null` propagates: every operation except `default_if_null` maps a `null`
/// source to `null`, as SQL does. Any other value the operation cannot handle
/// fails the row instead of being passed through or replaced by `null`.
fn transform_value(
    source: &Value,
    transform: &ResponseTransform,
) -> Result<Value, TransformFailure> {
    if transform.operation == "default_if_null" {
        return Ok(if source.is_null() {
            transform.value.clone().unwrap_or(Value::Null)
        } else {
            source.clone()
        });
    }
    if source.is_null() {
        return Ok(Value::Null);
    }
    let argument = || {
        transform
            .value
            .as_ref()
            .map(numeric_operand)
            .ok_or(TransformFailure::NotNumeric)?
    };
    let text = || {
        if is_text_scalar(source) {
            Ok(value_as_text(source))
        } else {
            Err(TransformFailure::NotText)
        }
    };
    let fixed_text = || {
        transform
            .value
            .as_ref()
            .map(value_as_text)
            .unwrap_or_default()
    };
    match transform.operation.as_str() {
        "subtract" => numeric_arithmetic(
            numeric_operand(source)?,
            argument()?,
            |a, b| checked_integer(a.checked_sub(b)),
            |a, b| a - b,
        ),
        "add" => numeric_arithmetic(
            numeric_operand(source)?,
            argument()?,
            |a, b| checked_integer(a.checked_add(b)),
            |a, b| a + b,
        ),
        "multiply" => numeric_arithmetic(
            numeric_operand(source)?,
            argument()?,
            |a, b| checked_integer(a.checked_mul(b)),
            |a, b| a * b,
        ),
        "divide" => {
            let divisor = argument()?;
            if divisor.is_zero() {
                // Rejected by validation; kept so this function never divides
                // by zero on its own.
                return Err(TransformFailure::Unrepresentable);
            }
            // Only an exact integer division stays integral; a remainder
            // means the true result is fractional, not unrepresentable.
            numeric_arithmetic(
                numeric_operand(source)?,
                divisor,
                |a, b| match a.checked_rem(b) {
                    Some(0) => checked_integer(a.checked_div(b)),
                    Some(_) => IntegerOutcome::Fractional,
                    None => IntegerOutcome::Unrepresentable,
                },
                |a, b| a / b,
            )
        }
        "round" => {
            let value = numeric_operand(source)?;
            if value.is_integer() {
                // Rounding an integer to any number of decimals is the integer.
                return value.into_value();
            }
            let decimals = transform
                .value
                .as_ref()
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(MAX_ROUND_DECIMALS);
            let factor = 10_f64.powi(decimals as i32);
            let value = value.exact_f64().ok_or(TransformFailure::Unrepresentable)?;
            float_value((value * factor).round() / factor)
        }
        "kelvin_to_celsius" | "celsius_to_kelvin" => {
            let value = numeric_operand(source)?
                .exact_f64()
                .ok_or(TransformFailure::Unrepresentable)?;
            let offset = 273.15;
            float_value(if transform.operation == "kelvin_to_celsius" {
                value - offset
            } else {
                value + offset
            })
        }
        "uppercase" => source
            .as_str()
            .map(|value| Value::String(value.to_uppercase()))
            .ok_or(TransformFailure::NotString),
        "lowercase" => source
            .as_str()
            .map(|value| Value::String(value.to_lowercase()))
            .ok_or(TransformFailure::NotString),
        "prefix" => Ok(Value::String(format!("{}{}", fixed_text(), text()?))),
        "suffix" => Ok(Value::String(format!("{}{}", text()?, fixed_text()))),
        "replace" => {
            let pair = transform.value.as_ref().and_then(Value::as_object);
            let find = pair.and_then(|pair| pair.get("find")?.as_str());
            let replacement = pair.and_then(|pair| pair.get("replace")?.as_str());
            match (find, replacement) {
                (Some(find), Some(replacement)) if !find.is_empty() => {
                    Ok(Value::String(text()?.replace(find, replacement)))
                }
                // Rejected by validation.
                _ => Err(TransformFailure::NotText),
            }
        }
        // Rejected by validation: never pass an unknown operation through.
        _ => Err(TransformFailure::NotText),
    }
}

fn render_template(
    template: &str,
    parameters: &JsonObject,
    url_encode: bool,
) -> (String, BTreeSet<String>) {
    let mut rendered = template.to_owned();
    let mut consumed = BTreeSet::new();
    for (key, value) in parameters {
        let placeholder = format!("{{{key}}}");
        if rendered.contains(&placeholder) {
            let text = value_as_text(value);
            let replacement = if url_encode {
                utf8_percent_encode(&text, PATH_SEGMENT_ENCODE_SET).to_string()
            } else {
                text
            };
            rendered = rendered.replace(&placeholder, &replacement);
            consumed.insert(key.clone());
        }
    }
    (rendered, consumed)
}

fn append_query(
    url: &mut Url,
    parameters: &JsonObject,
    specs: &[crate::ParameterSpec],
) -> Result<(), EngineError> {
    if parameters.is_empty() {
        return Ok(());
    }
    let mut pairs = Vec::new();
    for (name, value) in parameters {
        let serialization = specs
            .iter()
            .find(|spec| spec.name == *name)
            .and_then(|spec| spec.query_serialization.as_ref());
        serialize_query_parameter(&mut pairs, name, value, serialization)?;
    }
    let mut query = url.query_pairs_mut();
    for (name, value) in pairs {
        query.append_pair(&name, &value);
    }
    Ok(())
}

fn serialize_query_parameter(
    pairs: &mut Vec<(String, String)>,
    name: &str,
    value: &Value,
    serialization: Option<&QuerySerialization>,
) -> Result<(), EngineError> {
    let Some(serialization) = serialization else {
        match value {
            Value::Array(values) => {
                pairs.extend(
                    values
                        .iter()
                        .map(|value| (name.to_owned(), value_as_text(value))),
                );
            }
            value => pairs.push((name.to_owned(), value_as_text(value))),
        }
        return Ok(());
    };

    let explode = serialization
        .explode
        .unwrap_or(serialization.style == QueryStyle::Form);
    match (serialization.style, value) {
        (QueryStyle::Form, Value::Array(values)) if explode => {
            pairs.extend(
                values
                    .iter()
                    .map(|value| (name.to_owned(), value_as_text(value))),
            );
        }
        (QueryStyle::Form, Value::Array(values)) => {
            pairs.push((name.to_owned(), join_values(values, ",")));
        }
        (QueryStyle::Form, Value::Object(values)) if explode => {
            pairs.extend(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), value_as_text(value))),
            );
        }
        (QueryStyle::Form, Value::Object(values)) => {
            pairs.push((name.to_owned(), flatten_object(values, ",")));
        }
        (QueryStyle::SpaceDelimited, Value::Array(values)) => {
            pairs.push((name.to_owned(), join_values(values, " ")));
        }
        (QueryStyle::SpaceDelimited, Value::Object(values)) => {
            pairs.push((name.to_owned(), flatten_object(values, " ")));
        }
        (QueryStyle::PipeDelimited, Value::Array(values)) => {
            pairs.push((name.to_owned(), join_values(values, "|")));
        }
        (QueryStyle::PipeDelimited, Value::Object(values)) => {
            pairs.push((name.to_owned(), flatten_object(values, "|")));
        }
        (QueryStyle::DeepObject, Value::Object(values)) => {
            pairs.extend(
                values
                    .iter()
                    .map(|(key, value)| (format!("{name}[{key}]"), value_as_text(value))),
            );
        }
        (QueryStyle::DeepObject, _) => {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "deep_object query parameter must be an object",
            )));
        }
        (_, value) => pairs.push((name.to_owned(), value_as_text(value))),
    }
    Ok(())
}

fn join_values(values: &[Value], separator: &str) -> String {
    values
        .iter()
        .map(value_as_text)
        .collect::<Vec<_>>()
        .join(separator)
}

fn flatten_object(values: &JsonObject, separator: &str) -> String {
    values
        .iter()
        .flat_map(|(key, value)| [key.clone(), value_as_text(value)])
        .collect::<Vec<_>>()
        .join(separator)
}

fn parameter_header_value(value: &Value) -> String {
    match value {
        Value::Array(values) => join_values(values, ","),
        value => value_as_text(value),
    }
}

fn insert_header(
    headers: &mut std::collections::BTreeMap<String, String>,
    name: &str,
    value: String,
) {
    if let Some(existing) = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(name))
        .cloned()
    {
        headers.insert(existing, value);
    } else {
        headers.insert(name.to_owned(), value);
    }
}

fn cookie_value(value: &Value) -> Result<String, EngineError> {
    let value = parameter_header_value(value);
    if value
        .bytes()
        .any(|byte| byte <= 0x20 || byte >= 0x7f || matches!(byte, b';' | b','))
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "cookie parameter contains unsupported characters",
        )));
    }
    Ok(value)
}

fn validate_cookie_name(name: &str) -> Result<(), EngineError> {
    let valid = !name.is_empty()
        && name.bytes().all(|byte| {
            (0x21..=0x7e).contains(&byte)
                && !matches!(
                    byte,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        });
    if valid {
        Ok(())
    } else {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "invalid cookie parameter name",
        )))
    }
}

fn append_cookies(
    headers: &mut std::collections::BTreeMap<String, String>,
    cookies: &[(String, String)],
) {
    if cookies.is_empty() {
        return;
    }
    let value = cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let existing = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        .map(|(name, value)| (name.clone(), value.clone()));
    if let Some((name, existing)) = existing {
        headers.insert(name, format!("{existing}; {value}"));
    } else {
        headers.insert("Cookie".to_owned(), value);
    }
}

fn multipart_body(
    parameters: &JsonObject,
    max_request_bytes: usize,
) -> Result<PreparedBody, EngineError> {
    let mut fields = Vec::new();
    let mut files = Vec::new();
    let mut estimated_size = 0_usize;

    for (name, value) in parameters {
        if let Some(file) = value
            .as_object()
            .filter(|object| object.contains_key("data_base64"))
        {
            let encoded = file
                .get("data_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    EngineError::InvalidInput(ErrorDetail::from(
                        "multipart file has invalid data_base64",
                    ))
                })?;
            let data = STANDARD.decode(encoded).map_err(|_| {
                EngineError::InvalidInput(ErrorDetail::from("multipart file is not valid base64"))
            })?;
            let filename = file
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_owned();
            let content_type = file
                .get("content_type")
                .map(|value| {
                    value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        EngineError::InvalidInput(ErrorDetail::from(
                            "multipart file has invalid content_type",
                        ))
                    })
                })
                .transpose()?;
            estimated_size = estimated_size
                .saturating_add(data.len())
                .saturating_add(name.len())
                .saturating_add(filename.len())
                .saturating_add(1_024);
            ensure_request_size(estimated_size, max_request_bytes)?;
            files.push(PreparedFile {
                field_name: name.clone(),
                filename,
                content_type,
                source: PreparedFileSource::Bytes(data),
            });
        } else {
            let text = value_as_text(value);
            estimated_size = estimated_size
                .saturating_add(name.len())
                .saturating_add(text.len())
                .saturating_add(256);
            ensure_request_size(estimated_size, max_request_bytes)?;
            fields.push((name.clone(), text));
        }
    }

    Ok(PreparedBody::Multipart { fields, files })
}

fn ensure_request_size(size: usize, limit: usize) -> Result<(), EngineError> {
    if size > limit {
        Err(EngineError::RequestTooLarge { limit_bytes: limit })
    } else {
        Ok(())
    }
}

/// Refuses a `null` (alone, in an array, or as an object member) in a
/// parameter that is rendered as text: URL path, query, header, cookie, form
/// field, multipart field, or raw body template.
///
/// Text has no spelling for `null`. Rendering it as an empty string would send
/// a value the caller never gave, and dropping it would make it absent, so the
/// request is rejected before any network activity. A JSON body keeps `null`.
fn refuse_null_in_text(value: &Value) -> Result<(), EngineError> {
    fn contains_null(value: &Value) -> bool {
        match value {
            Value::Null => true,
            Value::Array(values) => values.iter().any(contains_null),
            Value::Object(values) => values.values().any(contains_null),
            Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
        }
    }
    if contains_null(value) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "parameter is null in a location rendered as text",
        )));
    }
    Ok(())
}

fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        value => Some(value_as_text(value)),
    }
}

fn usize_value(value: usize) -> Result<Value, EngineError> {
    let value = u64::try_from(value)
        .map_err(|_| EngineError::InvalidInput(ErrorDetail::from("numeric value is too large")))?;
    Ok(Value::Number(Number::from(value)))
}

fn ensure_page_size(page_size: usize) -> Result<(), EngineError> {
    if page_size == 0 {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "pagination page_size must be greater than zero",
        )))
    } else {
        Ok(())
    }
}

/// Appends at most `max_rows - output.len()` values; true when some were left
/// out, which means the source had more rows than the limit admits.
fn append_limited(output: &mut Vec<Value>, values: Vec<Value>, max_rows: usize) -> bool {
    let remaining = max_rows.saturating_sub(output.len());
    let available = values.len();
    output.extend(values.into_iter().take(remaining));
    available > remaining
}

fn pagination_limit(pagination: &PaginationConfig) -> EngineError {
    match pagination {
        PaginationConfig::Offset { max_rows, .. } | PaginationConfig::Page { max_rows, .. } => {
            EngineError::PaginationLimit {
                max_rows: *max_rows,
                max_pages: None,
            }
        }
        PaginationConfig::Cursor {
            max_rows,
            max_pages,
            ..
        }
        | PaginationConfig::Link {
            max_rows,
            max_pages,
            ..
        }
        | PaginationConfig::HeaderLink {
            max_rows,
            max_pages,
            ..
        } => EngineError::PaginationLimit {
            max_rows: *max_rows,
            max_pages: Some(*max_pages),
        },
    }
}

fn merge_execution_metrics(target: &mut ExecutionMetrics, source: &ExecutionMetrics) {
    target.requests = target.requests.saturating_add(source.requests);
    target.retries = target.retries.saturating_add(source.retries);
    target.auth_requests = target.auth_requests.saturating_add(source.auth_requests);
    target.poll_requests = target.poll_requests.saturating_add(source.poll_requests);
    target.cache_hits = target.cache_hits.saturating_add(source.cache_hits);
    target.cache_revalidations = target
        .cache_revalidations
        .saturating_add(source.cache_revalidations);
    target.rate_limit_wait_ms = target
        .rate_limit_wait_ms
        .saturating_add(source.rate_limit_wait_ms);
}

fn pagination_url(base: &Url, target: &str, allow_cross_origin: bool) -> Result<Url, EngineError> {
    let resolved = base
        .join(target)
        .map_err(|_| EngineError::InvalidResponse(ErrorDetail::from("invalid pagination link")))?;
    if !allow_cross_origin && !same_origin(base, &resolved) {
        return Err(EngineError::UnsafeAddress(ErrorDetail::from(
            "cross-origin pagination is blocked",
        )));
    }
    Ok(resolved)
}

pub(crate) fn link_header_target(
    headers: &BTreeMap<String, String>,
    expected_relation: &str,
) -> Result<Option<String>, EngineError> {
    let Some(header) = headers.get("link") else {
        return Ok(None);
    };
    for entry in split_link_header(header)? {
        let entry = entry.trim();
        let Some(target_end) = entry.strip_prefix('<').and_then(|value| value.find('>')) else {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "Link header contains an invalid target",
            )));
        };
        let target = &entry[1..=target_end];
        let parameters = &entry[target_end + 2..];
        for parameter in split_quoted(parameters, ';')? {
            let Some((name, value)) = parameter.split_once('=') else {
                continue;
            };
            if !name.trim().eq_ignore_ascii_case("rel") {
                continue;
            }
            let value = unquote_header_value(value.trim())?;
            if value
                .split_ascii_whitespace()
                .any(|relation| relation_matches(relation, expected_relation))
            {
                return Ok(Some(target.to_owned()));
            }
            // RFC 8288, 3.3: occurrences of rel after the first in a link
            // value are ignored, so `rel=prev; rel=next` is not a next link.
            break;
        }
    }
    Ok(None)
}

fn split_link_header(value: &str) -> Result<Vec<&str>, EngineError> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut in_target = false;
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes && character == '\\' {
            escaped = true;
            continue;
        }
        match character {
            '"' if !in_target => in_quotes = !in_quotes,
            '<' if !in_quotes => in_target = true,
            '>' if !in_quotes => in_target = false,
            ',' if !in_target && !in_quotes => {
                entries.push(&value[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if in_target || in_quotes || escaped {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "Link header is not well formed",
        )));
    }
    entries.push(&value[start..]);
    Ok(entries)
}

fn split_quoted(value: &str, separator: char) -> Result<Vec<&str>, EngineError> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes && character == '\\' {
            escaped = true;
        } else if character == '"' {
            in_quotes = !in_quotes;
        } else if character == separator && !in_quotes {
            values.push(&value[start..index]);
            start = index + character.len_utf8();
        }
    }
    if in_quotes || escaped {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "Link header parameter is not well formed",
        )));
    }
    values.push(&value[start..]);
    Ok(values)
}

/// The value of a Link parameter: a token as written, or a quoted-string with
/// its quoted-pairs resolved (RFC 9110, 5.6.4). Comparing the escaped text
/// would make `rel="n\ext"` miss `next` and end the pagination silently.
fn unquote_header_value(value: &str) -> Result<Cow<'_, str>, EngineError> {
    let invalid = || {
        EngineError::InvalidResponse(ErrorDetail::from(
            "Link header contains an invalid quoted value",
        ))
    };
    match (value.strip_prefix('"'), value.strip_suffix('"')) {
        (Some(inner), Some(_)) if inner.ends_with('"') => {
            let inner = &inner[..inner.len() - 1];
            if !inner.contains('\\') {
                return Ok(Cow::Borrowed(inner));
            }
            let mut unescaped = String::with_capacity(inner.len());
            let mut characters = inner.chars();
            while let Some(character) = characters.next() {
                if character == '\\' {
                    unescaped.push(characters.next().ok_or_else(invalid)?);
                } else {
                    unescaped.push(character);
                }
            }
            Ok(Cow::Owned(unescaped))
        }
        (None, None) => Ok(Cow::Borrowed(value)),
        _ => Err(invalid()),
    }
}

fn relation_matches(actual: &str, expected: &str) -> bool {
    if expected.contains(':') {
        actual == expected
    } else {
        actual.eq_ignore_ascii_case(expected)
    }
}

fn selected_response_headers(
    headers: &BTreeMap<String, String>,
    selected: &[String],
) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| {
            !is_sensitive_header_name(name)
                && (selected.iter().any(|name| name == "*")
                    || selected
                        .iter()
                        .any(|selected| selected.eq_ignore_ascii_case(name)))
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn ensure_success(
    connection: &ConnectionConfig,
    response: &ResponseData,
) -> Result<(), EngineError> {
    if (200..300).contains(&response.status)
        || connection.success_statuses.contains(&response.status)
    {
        return Ok(());
    }

    Err(EngineError::HttpStatus {
        status: response.status,
    })
}

fn apply_idempotency(
    connection: &ConnectionConfig,
    key: &str,
    query: &mut JsonObject,
    body: &mut JsonObject,
    headers: &mut BTreeMap<String, String>,
) -> Result<(), EngineError> {
    validate_idempotency_key(key)?;
    let name = connection.idempotency.name.trim();
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "idempotency field name must contain 1 to 256 non-control characters",
        )));
    }
    match connection.idempotency.location {
        IdempotencyLocation::Header => {
            if let Some(existing) = headers
                .iter()
                .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
            {
                if existing != key {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "idempotency header conflicts with a configured header",
                    )));
                }
            }
            insert_header(headers, name, key.to_owned());
        }
        IdempotencyLocation::Query => {
            insert_idempotency_field(query, name, key)?;
        }
        IdempotencyLocation::Body => {
            if matches!(connection.request.body_type, BodyType::None | BodyType::Raw) {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "body idempotency requires json, form_urlencoded, or multipart body",
                )));
            }
            insert_idempotency_field(body, name, key)?;
        }
    }
    Ok(())
}

fn insert_idempotency_field(
    target: &mut JsonObject,
    name: &str,
    key: &str,
) -> Result<(), EngineError> {
    if let Some(existing) = target.get(name) {
        if existing.as_str() != Some(key) {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "idempotency field conflicts with a configured parameter",
            )));
        }
    }
    target.insert(name.to_owned(), Value::String(key.to_owned()));
    Ok(())
}

fn public_response_url(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Authorization to carry the caller's credentials, scoped to the origin that
/// owns them.
///
/// Cross-origin follow-ups are opt-in, but authorizing the *navigation* is not
/// the same as authorizing the *forwarding of credentials*: a remote service
/// controlling a link, a `Location` header, or a result URL must not be able to
/// steer a bearer token, Basic credentials, an API key, an explicit cookie, or
/// an mTLS identity to an origin of its choosing.
///
/// The authorization is monotone. Once a chain has left the owning origin it
/// stays revoked for every descendant, including one that returns to the owning
/// origin: an `A -> B -> A` sequence would otherwise let B choose which
/// authenticated request the engine sends to A, which is a confused deputy even
/// though B never sees the secret itself.
#[derive(Clone)]
struct CredentialScope {
    origin: Url,
    /// Shared and write-once. Revocation reaches every holder of a clone,
    /// including a request that was already built — the remote cancellation is
    /// materialized when the job is registered, but may only be sent much
    /// later, after a further hop has revoked the authorization.
    revoked: Arc<AtomicBool>,
}

impl CredentialScope {
    fn new(origin: Url) -> Self {
        Self {
            origin,
            revoked: Arc::new(AtomicBool::new(false)),
        }
    }

    fn allows(&self, target: &Url) -> bool {
        !self.revoked.load(Ordering::Acquire) && same_origin(&self.origin, target)
    }

    /// Narrows the chain for a follow-up aimed at `target`.
    fn narrow(&self, target: &Url) {
        if !self.allows(target) {
            self.revoked.store(true, Ordering::Release);
        }
    }

    /// Strips the credentials from `request` unless this scope still authorizes
    /// them for its URL.
    ///
    /// `preserved` names a header that is not a credential and has to survive,
    /// so that dropping it cannot quietly weaken an unrelated guarantee.
    fn apply(&self, request: &mut PreparedRequest, preserved: Option<&str>) {
        if self.allows(&request.url) {
            return;
        }
        request.auth = AuthConfig::None;
        request.headers.retain(|name, _| {
            is_transferable_cross_origin_header(name)
                || preserved.is_some_and(|kept| kept.eq_ignore_ascii_case(name))
        });
        // The session's cookies do not cross the origin, but the caller's
        // handle stays in `caller_session`, so the transport still refuses the
        // request if the session has ended.
        request.cookies = CookiePolicy::default();
        request.tls.client_identity_pem = None;
    }
}

/// Name of the header carrying the idempotency key, when the engine adds one.
///
/// The key is generated by the caller and is not a credential, but dropping it
/// on a cross-origin page would silently break a safety property: an idempotency
/// key also switches on retries for non-idempotent methods, so a request that
/// lost the key would still be retried, without the protection the key gives.
/// Keeping it also matches the query and body locations, which are part of the
/// URL or the payload and were never stripped.
fn preserved_idempotency_header<'a>(
    connection: &'a ConnectionConfig,
    options: &crate::ExecutionOptions,
) -> Option<&'a str> {
    if options.idempotency_key.is_none()
        || connection.idempotency.location != IdempotencyLocation::Header
    {
        return None;
    }
    let name = connection.idempotency.name.trim();
    // Both conditions matter: the name has to earn the exception by naming an
    // idempotency key, and it must not read as a credential.
    if !names_an_idempotency_key(name) || is_disallowed_idempotency_header(name) {
        return None;
    }
    Some(name)
}

/// True when the engine added an idempotency header and the credential scope
/// removed it from `request`.
fn idempotency_header_withheld(
    connection: &ConnectionConfig,
    options: &crate::ExecutionOptions,
    request: &PreparedRequest,
) -> bool {
    if options.idempotency_key.is_none()
        || connection.idempotency.location != IdempotencyLocation::Header
    {
        return false;
    }
    let name = connection.idempotency.name.trim();
    !request
        .headers
        .keys()
        .any(|header| header.eq_ignore_ascii_case(name))
}

/// True when an idempotency header name would smuggle a credential-shaped
/// header past the checks that exist for exactly that.
///
/// The name is caller configured, so the allowlist exception must not become a
/// way to widen the allowlist: naming the idempotency header `Authorization`
/// would otherwise forward an auth header to an origin the remote service
/// chose. The exception is therefore granted only to a name that says it
/// carries an idempotency key — which the default `Idempotency-Key` does, even
/// though the credential classifier flags its `key` component.
pub(crate) fn is_disallowed_idempotency_header(name: &str) -> bool {
    // Trimmed first: `apply_idempotency` trims before inserting the header, so
    // judging the untrimmed name would let `" Authorization "` through and still
    // emit a real `Authorization` header.
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return true;
    }
    if has_credential_compound(&name) {
        return true;
    }
    // Only an exact idempotency word is discounted, and `key` only alongside
    // one. A component that merely begins with it — `IdempotencyToken`,
    // `IdempotencyApiKey` — is judged like any other.
    let marks_idempotency = header_components(&name).any(is_idempotency_word);
    header_components(&name)
        .filter(|component| {
            !is_idempotency_word(component) && !(marks_idempotency && *component == "key")
        })
        .any(is_credential_component)
}

fn is_idempotency_word(component: &str) -> bool {
    matches!(component, "idempotency" | "idempotence" | "idempotent")
}

/// True when the name positively claims to carry an idempotency *key*.
///
/// Required before the cross-origin allowlist exception is granted. Rejecting
/// credential-shaped names is not enough on its own: it would still let an
/// arbitrary header such as `X-Vendor-Code` ride across an origin carrying a
/// caller-controlled value, and `Idempotency-Status` names a status, not a key.
fn names_an_idempotency_key(name: &str) -> bool {
    /// Names that are idempotency keys by convention without saying so.
    const ALIASES: [&str; 2] = ["request-id", "x-request-id"];

    let name = name.trim().to_ascii_lowercase();
    if ALIASES.contains(&name.as_str()) {
        return true;
    }
    let components = header_components(&name).collect::<Vec<_>>();
    let glued = ["idempotencykey", "idempotencekey", "idempotentkey"];
    components.iter().any(|component| glued.contains(component))
        || (components.iter().copied().any(is_idempotency_word) && components.contains(&"key"))
}

/// Whole header-name components that read as credentials.
const CREDENTIAL_WORDS: [&str; 15] = [
    "authorization",
    "bearer",
    "cookie",
    "credential",
    "credentials",
    "jwt",
    "key",
    "passcode",
    "passphrase",
    "passwd",
    "password",
    "secret",
    "session",
    "signature",
    "token",
];

/// One-time-code stems, matched at either end of a component.
///
/// These letter sequences do not open or close any ordinary English word, so
/// matching both ends catches `X-OTP`, `X-OTPCode` and `X-VendorOTP` alike
/// without enumerating the words that can sit next to them — and a new spelling
/// nobody anticipated is caught by the same rule.
const ONE_TIME_CODE_STEMS: [&str; 3] = ["hotp", "otp", "totp"];

/// Words that end a component's meaning. Matched as suffixes so a glued name
/// such as `IdempotencyToken` or `X-SessionToken` is still recognised, while a
/// word that merely *starts* with one — `Secretariat` — is not.
///
/// `key` is deliberately absent: it would classify `X-Monkey`.
const CREDENTIAL_SUFFIXES: [&str; 13] = [
    "accesskey",
    "apikey",
    "authorization",
    "credential",
    "jwt",
    "passcode",
    "passphrase",
    "passwd",
    "password",
    "privatekey",
    "secret",
    "signature",
    "token",
];

/// Compound markers spelled across adjacent components, as in `X-Api-Key`, or
/// glued into one, as in `X-CsrfToken`.
///
/// The `*key` entries look redundant next to the generic `key` handling, but
/// they are not: when `key` is its own component the idempotency rule discounts
/// it, and only the pairing with the component before it — `api` + `key` —
/// still says credential.
const CREDENTIAL_COMPOUNDS: [&str; 16] = [
    "accesskey",
    "accesstoken",
    "apikey",
    "authtoken",
    "clientkey",
    "clientsecret",
    "clienttoken",
    "consumerkey",
    "csrftoken",
    "encryptionkey",
    "idtoken",
    "masterkey",
    "privatekey",
    "refreshtoken",
    "secretkey",
    "sessionid",
];

/// Ordinary English words ending in `key`.
///
/// Anything else ending in `key` is treated as a credential. Enumerating the
/// benign side is what makes this robust: a missing entry here over-classifies
/// a header, while a missing entry in a list of credential spellings would let
/// one through — `X-AppKey`, `X-AccountKey`, `X-ServiceKey` and their kin are
/// endless, and each omission is a leak.
///
/// Only ordinary words belong here. A vendor identifier goes in
/// `BENIGN_HEADER_NAMES` instead: exempting a bare component would exempt every
/// header that happens to contain it, including ones nobody has looked at.
const BENIGN_KEY_WORDS: [&str; 7] = [
    "donkey", "hockey", "jockey", "malarkey", "monkey", "turkey", "whiskey",
];

/// Complete header names that end in `key` but identify routing or
/// partitioning, not a secret.
///
/// Matched in full, not by component, so the exemption covers exactly the
/// header it was verified for. Cosmos DB requires both of these on ordinary
/// document operations. Add an entry only with that kind of evidence, and only
/// as a whole name.
const BENIGN_HEADER_NAMES: [&str; 2] = [
    "x-ms-documentdb-partitionkey",
    "x-ms-documentdb-raw-partitionkey",
];

/// Splits a header name into its components.
///
/// Every character that cannot appear inside a word is a separator, not just
/// `-` and `_`: the HTTP token grammar also allows `.`, `!`, `+`, `~` and more,
/// so `X.Token` is a legal name that has to be classified like `X-Token`.
fn header_components(name: &str) -> impl Iterator<Item = &str> {
    name.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|component| !component.is_empty())
}

fn is_credential_component(component: &str) -> bool {
    // A trailing plural is stripped before matching, so `X-Api-Keys` and
    // `X-Access-Tokens` are read exactly like their singular spellings.
    let stem = component.strip_suffix('s').unwrap_or(component);
    if stem.ends_with("key") && !BENIGN_KEY_WORDS.contains(&stem) {
        return true;
    }
    if ONE_TIME_CODE_STEMS
        .iter()
        .any(|code| stem.starts_with(code) || stem.ends_with(code))
    {
        return true;
    }
    if CREDENTIAL_WORDS.contains(&component)
        || CREDENTIAL_WORDS.contains(&stem)
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| component.ends_with(suffix) || stem.ends_with(suffix))
        || component.starts_with("authenticat")
    {
        return true;
    }
    // `auth*` reads as a credential — `authn`, `authz`, `authtoken` — with the
    // `author*` family as the one ordinary exception, which `authoriz*` is not
    // part of despite sharing the prefix.
    component.starts_with("auth")
        && (!component.starts_with("author") || component.starts_with("authoriz"))
}

/// True when a compound marker is spelled out by a run of adjacent components,
/// or ends one of them.
///
/// A trailing plural is stripped as for single components, so `X-SessionIDs`
/// and `X-Session-Ids` read like `X-SessionID`.
fn has_credential_compound(name: &str) -> bool {
    let singular = |text: &str| text.strip_suffix('s').map(str::to_owned);
    let is_marker = |text: &str| {
        CREDENTIAL_COMPOUNDS.contains(&text)
            || singular(text).is_some_and(|stem| CREDENTIAL_COMPOUNDS.contains(&stem.as_str()))
    };
    let components = header_components(name).collect::<Vec<_>>();
    if components.iter().any(|component| {
        let stem = component.strip_suffix('s').unwrap_or(component);
        CREDENTIAL_COMPOUNDS
            .iter()
            .any(|marker| component.ends_with(marker) || stem.ends_with(marker))
    }) {
        return true;
    }
    (0..components.len()).any(|start| {
        let mut joined = String::new();
        components.iter().skip(start).any(|component| {
            joined.push_str(component);
            is_marker(&joined)
        })
    })
}

/// Conservative classification of header names that may carry credentials.
///
/// An exact blacklist cannot cover vendor specific names such as `X-Auth-Token`
/// or `X-Amz-Security-Token`, so the classification is intentionally
/// over-inclusive. It governs which headers are withheld from public results
/// and which inline headers the runtime boundary refuses.
///
/// It deliberately does *not* decide what crosses an origin: forwarding uses
/// `is_transferable_cross_origin_header`, an allowlist, because there a single
/// unrecognised name would be a leak rather than a missed redaction.
pub(crate) fn is_sensitive_header_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if BENIGN_HEADER_NAMES.contains(&name.as_str()) {
        return false;
    }
    has_credential_compound(&name) || header_components(&name).any(is_credential_component)
}

/// Request headers that may follow a request to a *different* origin.
///
/// Forwarding is an allowlist rather than a blocklist on purpose. Classifying
/// credentials by pattern is adequate for redaction, where a miss only reveals
/// a header the caller already sent to that origin, but it is the wrong shape
/// here: one unrecognised vendor header name is enough to hand a secret to an
/// origin the remote service picked. Only headers describing the representation
/// the caller asked for are carried across.
fn is_transferable_cross_origin_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept"
            | "accept-charset"
            | "accept-encoding"
            | "accept-language"
            | "cache-control"
            | "content-type"
            | "if-match"
            | "if-modified-since"
            | "if-none-match"
            | "if-range"
            | "if-unmodified-since"
            | "pragma"
            | "range"
            | "user-agent"
    )
}

fn failed_result(error: EngineError) -> ExecutionResult {
    failed_result_with_recoveries(error, RecoveryHandles::default())
}

fn failed_result_with_recoveries(
    error: EngineError,
    recoveries: RecoveryHandles,
) -> ExecutionResult {
    with_recoveries(
        ExecutionResult {
            schema_version: SCHEMA_VERSION,
            status: ExecutionStatus::Failed,
            output: ExecutionOutput::None,
            metrics: ExecutionMetrics::default(),
            responses: Vec::new(),
            errors: vec![error.execution_error(None)],
            recoveries: Vec::new(),
        },
        recoveries,
    )
}

/// Puts the recovery handles into `result`, saying how many did not fit.
///
/// The contract caps `recoveries` at [`MAX_RECOVERIES`]. A handle left out is
/// a remote job the caller can no longer resume, so the cut is never silent:
/// the first error carries `recoveries_omitted` with the number left out. A
/// result with omitted handles and no error cannot be produced by the engine
/// (a job still running always comes with the failure that interrupted it);
/// it is reported as an internal error rather than as a clean success.
fn with_recoveries(mut result: ExecutionResult, recoveries: RecoveryHandles) -> ExecutionResult {
    result.recoveries = recoveries.handles;
    if recoveries.omitted > 0 {
        let omitted = Value::from(u64::try_from(recoveries.omitted).unwrap_or(u64::MAX));
        if result.errors.is_empty() {
            result.errors.push(
                EngineError::Runtime(ErrorDetail::from(
                    "recovery handles were omitted without a failure",
                ))
                .execution_error(None),
            );
            if result.status == ExecutionStatus::Success {
                result.status = ExecutionStatus::Partial;
            }
        }
        if let Some(first) = result.errors.first_mut() {
            first
                .details
                .insert("recoveries_omitted".to_owned(), omitted);
        }
    }
    result
}

fn validate_idempotency_key(key: &str) -> Result<(), EngineError> {
    if key.is_empty() || key.len() > 255 || !key.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "idempotency_key must contain 1 to 255 visible ASCII bytes",
        )));
    }
    Ok(())
}

fn validate_execution_configuration(request: &ExecutionRequest) -> Result<(), EngineError> {
    validate_transforms(&request.connection.response)?;
    validate_json_paths(&request.connection)?;
    let Some(polling) = request.connection.polling.as_ref() else {
        return Ok(());
    };
    if polling.max_attempts == 0 {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling max_attempts must be greater than zero",
        )));
    }
    if polling.max_wait_ms == Some(0) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling max_wait_ms must be greater than zero when configured",
        )));
    }
    if !polling.interval_backoff.is_finite() || polling.interval_backoff < 1.0 {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling interval_backoff must be finite and at least one",
        )));
    }
    if polling
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.timeout_ms == 0)
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling cancel timeout_ms must be greater than zero",
        )));
    }

    let Some(resume) = polling.resume.as_ref() else {
        return Ok(());
    };
    validate_job_id(&resume.job_id)?;
    if polling.url_template.is_none() {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling resume requires url_template and cannot infer a prior Location URL",
        )));
    }
    if request.connection.pagination.is_some() {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling resume cannot be combined with pagination",
        )));
    }
    if request
        .connection
        .batch
        .as_ref()
        .is_some_and(|batch| batch.enabled)
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling resume cannot be combined with batch execution",
        )));
    }
    if request.operation == ExecutionOperation::Enrich && request.input.records.len() != 1 {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling resume for enrichment requires exactly one input record",
        )));
    }
    Ok(())
}

/// Every JSON path a request reads from a response must be well formed.
///
/// At run time a path that does not resolve means "the response does not have
/// it", which the operation may legitimately turn into null or a default. A
/// malformed path never resolves, so without this check a typo in the
/// configuration would be indistinguishable from a response that lacks the
/// field.
fn validate_json_paths(connection: &ConnectionConfig) -> Result<(), EngineError> {
    let response = &connection.response;
    let mut paths: Vec<&str> = Vec::new();
    paths.extend(response.records_path.as_deref());
    paths.extend(response.error_path.as_deref());
    paths.extend(
        response
            .output_mapping
            .iter()
            .map(|mapping| mapping.path.as_str()),
    );
    paths.extend(
        response
            .iterate_on
            .iter()
            .map(|iteration| iteration.path.as_str()),
    );
    if let Some(batch) = connection.batch.as_ref() {
        paths.push(&batch.output_path);
    }
    if let Some(polling) = connection.polling.as_ref() {
        paths.push(&polling.id_path);
        paths.push(&polling.status_path);
        paths.extend(polling.url_path.as_deref());
        paths.extend(polling.result_path.as_deref());
        paths.extend(polling.result_url_path.as_deref());
    }
    match connection.pagination.as_ref() {
        Some(PaginationConfig::Cursor { cursor_path, .. }) => paths.push(cursor_path),
        Some(PaginationConfig::Link { link_path, .. }) => paths.push(link_path),
        _ => {}
    }
    if paths.into_iter().all(json_path::is_valid) {
        Ok(())
    } else {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "connection contains a malformed JSON path",
        )))
    }
}

fn execution_fingerprint(request: &ExecutionRequest) -> Result<String, EngineError> {
    let mut normalized = request.clone();
    normalized.options.deadline = None;
    normalized.options.idempotency_key = None;
    let bytes = serde_json::to_vec(&normalized).map_err(|_| {
        EngineError::Runtime(ErrorDetail::from(
            "request could not be serialized for its fingerprint",
        ))
    })?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn scoped_execution_options(
    options: &crate::ExecutionOptions,
    index: usize,
) -> crate::ExecutionOptions {
    let mut scoped = options.clone();
    scoped.idempotency_key = options.idempotency_key.as_deref().map(|key| {
        let mut digest = Sha256::new();
        digest.update(b"plenora-rest-idempotency-child-v1\0");
        digest.update(key.as_bytes());
        digest.update(b"\0");
        digest.update(index.to_string().as_bytes());
        format!("plenora-{:x}", digest.finalize())
    });
    scoped
}

fn cancel_enabled(cancel: &ActiveRemoteCancel, trigger: RemoteCancelTrigger) -> bool {
    match trigger {
        RemoteCancelTrigger::Cancellation => cancel.on_cancellation,
        RemoteCancelTrigger::Deadline => cancel.on_deadline,
        RemoteCancelTrigger::PollTimeout => cancel.on_poll_timeout,
    }
}

fn active_jobs_handle() -> Option<Arc<Mutex<BTreeMap<String, ActiveAsyncJob>>>> {
    ACTIVE_ASYNC_JOBS.try_with(Arc::clone).ok()
}

fn register_active_job(key: String, job: ActiveAsyncJob) {
    let Some(jobs) = active_jobs_handle() else {
        return;
    };
    jobs.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, job);
}

fn remove_active_job(key: &str) {
    let Some(jobs) = active_jobs_handle() else {
        return;
    };
    jobs.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}

fn active_recoveries() -> RecoveryHandles {
    active_jobs_handle()
        .map(|jobs| recoveries_from(&jobs))
        .unwrap_or_default()
}

/// Contract bound on `recoveries` in the execution and file transfer results.
const MAX_RECOVERIES: usize = 128;

/// The recovery handles a result can carry, and how many did not fit.
#[derive(Default)]
struct RecoveryHandles {
    handles: Vec<AsyncJobRecovery>,
    omitted: usize,
}

fn recoveries_from(jobs: &Arc<Mutex<BTreeMap<String, ActiveAsyncJob>>>) -> RecoveryHandles {
    // The map is keyed by poll URL, so the cut at the contract bound is
    // deterministic rather than dependent on iteration order.
    let mut handles = jobs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter_map(|job| job.recovery.clone())
        .collect::<Vec<_>>();
    let omitted = handles.len().saturating_sub(MAX_RECOVERIES);
    handles.truncate(MAX_RECOVERIES);
    RecoveryHandles { handles, omitted }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{
        evaluate_application_success, is_disallowed_idempotency_header, is_sensitive_header_name,
        link_header_target, map_value, names_an_idempotency_key, render_template,
        resolve_parameters, selected_response_headers,
    };
    use crate::{ConnectionConfig, OutputMapping, ParameterLocation, ParameterMode, ParameterSpec};

    #[test]
    fn renders_url_parameters_and_tracks_consumed_keys() {
        let parameters = json!({"id": "a/b", "query": "rust"})
            .as_object()
            .unwrap()
            .clone();
        let (url, consumed) = render_template("https://example.com/{id}", &parameters, true);
        assert_eq!(url, "https://example.com/a%2Fb");
        assert!(consumed.contains("id"));
        assert!(!consumed.contains("query"));
    }

    #[test]
    fn resolves_mapped_and_fixed_parameters() {
        let connection = ConnectionConfig {
            parameters: vec![
                ParameterSpec {
                    name: "user_id".to_owned(),
                    mode: ParameterMode::Mapped,
                    source: Some("user.id".to_owned()),
                    value: None,
                    required: true,
                    location: ParameterLocation::Auto,
                    query_serialization: None,
                },
                ParameterSpec {
                    name: "limit".to_owned(),
                    mode: ParameterMode::Fixed,
                    source: None,
                    value: Some(json!(10)),
                    required: false,
                    location: ParameterLocation::Auto,
                    query_serialization: None,
                },
            ],
            ..ConnectionConfig::default()
        };
        let source = json!({"user": {"id": 42}}).as_object().unwrap().clone();
        let parameters = resolve_parameters(&connection, &source).unwrap();
        assert_eq!(parameters["user_id"], json!(42));
        assert_eq!(parameters["limit"], json!(10));
    }

    #[test]
    fn maps_response_fields() {
        let mappings = vec![OutputMapping {
            path: "profile.name".to_owned(),
            column: "full_name".to_owned(),
            default: None,
        }];
        let mapped = map_value(&json!({"profile": {"name": "Ada"}}), &mappings);
        assert_eq!(mapped["full_name"], json!("Ada"));
    }

    #[test]
    fn parses_link_headers_with_commas_and_multiple_relations() {
        let headers = BTreeMap::from([(
            "link".to_owned(),
            r#"<https://example.test/items?cursor=a,b>; rel="next alternate", </last>; rel=last"#
                .to_owned(),
        )]);
        assert_eq!(
            link_header_target(&headers, "NEXT").unwrap().as_deref(),
            Some("https://example.test/items?cursor=a,b")
        );
        assert_eq!(
            link_header_target(&headers, "last").unwrap().as_deref(),
            Some("/last")
        );
    }

    #[test]
    fn a_quoted_relation_is_compared_without_its_escapes() {
        // Found by the property test against RFC 8288: the quoted-pair was
        // compared as written, so the next page was never found and the
        // pagination ended as if the service had no more pages.
        let headers = BTreeMap::from([(
            "link".to_owned(),
            r#"</a>; rel="n\ext", </b>; rel="http://example.com/\Rel""#.to_owned(),
        )]);
        assert_eq!(
            link_header_target(&headers, "next").unwrap().as_deref(),
            Some("/a")
        );
        assert_eq!(
            link_header_target(&headers, "http://example.com/Rel")
                .unwrap()
                .as_deref(),
            Some("/b")
        );
    }

    #[test]
    fn only_the_first_rel_parameter_of_a_link_counts() {
        // Found by the property test against RFC 8288: a second rel in the
        // same link value was honoured, so a link declared as prev was
        // followed as the next page.
        let headers = BTreeMap::from([(
            "link".to_owned(),
            "</prev>; rel=prev; rel=next, </next>; rel=next".to_owned(),
        )]);
        assert_eq!(
            link_header_target(&headers, "next").unwrap().as_deref(),
            Some("/next")
        );
        let headers =
            BTreeMap::from([("link".to_owned(), "</prev>; rel=prev; rel=next".to_owned())]);
        assert_eq!(link_header_target(&headers, "next").unwrap(), None);
    }

    #[test]
    fn idempotency_header_exception_is_granted_only_to_idempotency_names() {
        for name in [
            "Idempotency-Key",
            " Idempotency-Key ",
            "idempotence-key",
            "X-Idempotency-Key",
            "Request-Id",
        ] {
            assert!(
                !is_disallowed_idempotency_header(name),
                "{name} names an idempotency key and must keep the exception"
            );
        }
        // The name is caller configured, so the exception must not become a way
        // of forwarding an auth header. Whitespace matters because
        // `apply_idempotency` trims before inserting the header.
        for name in [
            "Authorization",
            " Authorization ",
            "Authorization-Idempotency",
            "Idempotency-Authorization",
            "Cookie",
            "X-Idempotency-Token",
            "Proxy-Authorization",
            "   ",
            // Gluing the marker onto a credential, or splitting a compound
            // marker with it, must not buy the exception either.
            "AuthorizationIdempotency",
            "X-AuthIdempotency",
            "X-Api-Key-Idempotency",
            "Idempotency-Private-Key",
            "Idempotency-Session-Id",
            // The HTTP token grammar allows more than `-` and `_`, so a name
            // using other legal punctuation must be classified the same way.
            "Idempotency.Token",
            "X!Token",
            // Gluing the marker to the credential must not swallow it either.
            "IdempotencyToken",
            "IdempotencyAuthorization",
            "IdempotencyApiKey",
            "IdempotencyPassword",
        ] {
            assert!(
                is_disallowed_idempotency_header(name),
                "{name} reads as a credential and must lose the exception"
            );
        }
    }

    #[test]
    fn the_cross_origin_exception_requires_naming_an_idempotency_key() {
        // Not being credential-shaped is not enough: the exception has to be
        // earned, or any header name would ride across an origin carrying a
        // caller-controlled value.
        for name in [
            "Idempotency-Key",
            "X-Idempotency-Key",
            "IdempotencyKey",
            "Request-Id",
        ] {
            assert!(
                names_an_idempotency_key(name),
                "{name} names an idempotency key"
            );
        }
        for name in [
            "X-Vendor-Code",
            "X-Trace",
            "Authorization",
            // Mentioning idempotency is not the same as naming a key.
            "Idempotency-Status",
        ] {
            assert!(
                !names_an_idempotency_key(name),
                "{name} must not earn the cross-origin exception"
            );
        }
    }

    #[test]
    fn sensitive_header_classification_matches_components_not_substrings() {
        for name in [
            "Authorization",
            "Proxy-Authorization",
            "WWW-Authenticate",
            "Cookie",
            "Set-Cookie",
            "X-Api-Key",
            "X-ApiKey",
            "X-Auth-Token",
            "X-Amz-Security-Token",
            "X-Secret",
            "X-Session-Id",
            "X-Request-Signature",
            "X_Access_Token",
            // Other legal HTTP token punctuation separates components too.
            "X.Token",
            "X!Api!Key",
            // Concatenated vendor spellings.
            "X-SessionToken",
            "X-ClientToken",
            "X-JWT",
            "X-Passphrase",
            // `key` is not a generic suffix, so the credential-bearing
            // spellings are enumerated; `X-Monkey` below must stay benign.
            "X-ClientKey",
            "X-ConsumerKey",
            "X-SessionKey",
            "X-SecretKey",
            "X-SigningKey",
            "X-SubscriptionKey",
            "X-AppKey",
            "X-AccountKey",
            "X-ServiceKey",
            "X-EncryptionKey",
            // A passkey is an authentication credential.
            "Passkey",
            "X-Passkey",
            "X-Passcode",
            // A one-time code is caught wherever its stem sits in the
            // component: alone, at the front, or at the end.
            "X-OTP",
            "X-TOTP",
            "X-HOTP",
            "X-OTPCode",
            "X-TOTPCode",
            "X-HOTPCode",
            "X-OTPValue",
            "X-VendorOTP",
            "X-Vendor-TOTP",
            "X-OTP-Code",
            // Plural spellings name the same credentials.
            "X-Api-Keys",
            "X-Access-Tokens",
            "X-Secrets",
            "X-SessionIDs",
            "X-Session-Ids",
            "X-Refresh-Tokens",
            "X-ClientSecrets",
            // The routing exemption is granted to verified full names only, so
            // an unrelated header carrying the same component keeps the
            // conservative treatment.
            "X-PartitionKey",
            "X-Master-PartitionKey",
        ] {
            assert!(is_sensitive_header_name(name), "{name} must be sensitive");
        }
        // Substring matching would classify these as credentials and either
        // reject a legitimate runtime request or silently drop the header.
        for name in [
            "ETag",
            "Content-Type",
            "X-RateLimit-Remaining",
            "Keep-Alive",
            "X-Author",
            "X-Authored-By",
            "X-Monkey",
            "X-Monkeys",
            "Retry-After",
            // Compound markers are matched against whole components, so a word
            // that merely contains one is not a credential.
            "X-Secretariat",
            "X-Tokenizer",
            // Documented routing identifiers, not secrets: Cosmos DB requires
            // them on ordinary document operations. Exempted by full name, so
            // the exemption covers exactly what was verified.
            "x-ms-documentdb-partitionkey",
            "x-ms-documentdb-raw-partitionkey",
        ] {
            assert!(
                !is_sensitive_header_name(name),
                "{name} must not be classified as a credential"
            );
        }
    }

    #[test]
    fn response_header_capture_never_exposes_sensitive_values() {
        let headers = BTreeMap::from([
            ("etag".to_owned(), "abc".to_owned()),
            ("set-cookie".to_owned(), "secret=1".to_owned()),
        ]);
        assert_eq!(
            selected_response_headers(&headers, &["ETag".to_owned()]),
            BTreeMap::from([("etag".to_owned(), "abc".to_owned())])
        );
        assert_eq!(
            selected_response_headers(&headers, &["*".to_owned()]),
            BTreeMap::from([("etag".to_owned(), "abc".to_owned())])
        );
    }

    #[test]
    fn the_remote_message_at_error_path_is_not_captured() {
        let connection = ConnectionConfig {
            response: crate::ResponseConfig {
                error_path: Some("error".to_owned()),
                ..crate::ResponseConfig::default()
            },
            ..ConnectionConfig::default()
        };
        let response = json!({"error": {"message": "user abc123 not found at 10.0.0.7"}});
        let Err(crate::EngineError::Application(detail)) =
            evaluate_application_success(&response, &connection)
        else {
            panic!("an error at error_path must fail the request");
        };
        assert_eq!(detail.text(), "the response reports an error at error_path");
        let debug = format!("{detail:?}");
        assert!(
            !debug.contains("abc123") && !debug.contains("10.0.0.7"),
            "{debug}"
        );
    }
}
