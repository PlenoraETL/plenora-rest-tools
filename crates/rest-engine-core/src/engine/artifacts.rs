//! File-transfer admission, source integrity and operation outputs.

use super::{
    AsyncReadExt, CredentialScope, Digest, DownloadTarget, Engine, EngineConfig, EngineError,
    ErrorDetail, ExecutionMetrics, ExecutionOutput, ExecutionRequest, FileTransferDirection,
    FileTransferInput, HttpResponseMetadata, IntegrityMetadata, OperationResult, Path, PathBuf,
    PreparedBody, PreparedFile, PreparedFileSource, PreparedStream, Sha256, fs,
    public_response_url, remove_active_job, resolve_parameters, selected_response_headers,
};
use crate::error::file_io;
impl Engine {
    pub(super) async fn download(
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

    pub(super) async fn upload(
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
        if let Some(expected) = validated_checksum(file.expected_sha256.as_deref())?
            && !sha256.eq_ignore_ascii_case(&expected)
        {
            return Err(EngineError::ChecksumMismatch);
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
}

impl Engine {
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
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
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
