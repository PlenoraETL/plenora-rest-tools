use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{StreamExt, stream};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Url;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncReadExt, time::sleep};

use crate::{
    ASYNC_JOB_RECOVERY_CONTRACT, AsyncJobRecovery, BatchConfig, BatchInputFormat, BodyType,
    CachePolicy, CancellationToken, CapabilityDocument, ConnectionConfig, EngineConfig,
    EngineError, ExecutionControl, ExecutionError, ExecutionMetrics, ExecutionOperation,
    ExecutionOutput, ExecutionRequest, ExecutionResult, ExecutionStatus, FileTransferDirection,
    FileTransferInput, HttpMethod, HttpResponseMetadata, IdempotencyLocation, IntegrityMetadata,
    JsonObject, OutputMapping, PaginationConfig, ParameterLocation, ParameterMode,
    PollingCancelConfig, PollingConfig, QuerySerialization, QueryStyle, ResponseConfig,
    ResponseTransform, SCHEMA_VERSION, capabilities, json_path, response_body,
    transport::{
        DownloadTarget, PreparedBody, PreparedFile, PreparedFileSource, PreparedRequest,
        PreparedStream, ResponseData, Transport, same_origin,
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
        let control = match ExecutionControl::default()
            .with_optional_deadline(request.options.deadline.as_deref())
        {
            Ok(control) => control,
            Err(error) => return failed_result(error),
        };
        self.execute_with_control(request, control).await
    }

    pub async fn execute_with_control(
        &self,
        request: ExecutionRequest,
        control: ExecutionControl,
    ) -> ExecutionResult {
        if self.is_closed() {
            return failed_result(EngineError::EngineClosed);
        }
        if let Err(error) = validate_execution_configuration(&request) {
            return failed_result(error);
        }
        if let Err(error) = self.admit_idempotency(&request) {
            return failed_result(error);
        }
        if control
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            return failed_result(EngineError::Timeout);
        }
        let active_jobs = Arc::new(Mutex::new(BTreeMap::new()));
        let deadline = control.deadline.map(tokio::time::Instant::from_std);
        let execution = ACTIVE_ASYNC_JOBS.scope(active_jobs.clone(), self.execute_inner(request));
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
        match outcome {
            Controlled::Finished(result) => result,
            Controlled::Cancelled => {
                self.cancel_active_jobs(&active_jobs, RemoteCancelTrigger::Cancellation)
                    .await;
                failed_result_with_recoveries(EngineError::Cancelled, recoveries_from(&active_jobs))
            }
            Controlled::Deadline => {
                self.cancel_active_jobs(&active_jobs, RemoteCancelTrigger::Deadline)
                    .await;
                failed_result_with_recoveries(EngineError::Timeout, recoveries_from(&active_jobs))
            }
        }
    }

    pub fn capabilities(&self) -> CapabilityDocument {
        capabilities()
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
            return Err(EngineError::PolicyViolation(
                "max_idempotency_keys must be greater than zero when a key is used".to_owned(),
            ));
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
                    Some((key.clone(), cancel.request.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (key, request) in requests {
            let accepted = self
                .transport
                .execute(request)
                .await
                .is_ok_and(|response| (200..300).contains(&response.status));
            let mut jobs = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(recovery) = jobs.get_mut(&key).and_then(|job| job.recovery.as_mut()) {
                recovery.cancel_accepted = Some(accepted);
            }
        }
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
            cancel.request.clone()
        };
        let accepted = self
            .transport
            .execute(request)
            .await
            .is_ok_and(|response| (200..300).contains(&response.status));
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
            Err(EngineError::InvalidInput(
                "credential_ref requires an authorized runtime resolver".to_owned(),
            ))
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
                ExecutionResult {
                    schema_version: SCHEMA_VERSION,
                    status,
                    output: operation.output,
                    metrics,
                    responses,
                    errors: operation.errors,
                    recoveries: active_recoveries(),
                }
            }
            Err(error) => ExecutionResult {
                schema_version: SCHEMA_VERSION,
                status: ExecutionStatus::Failed,
                output: ExecutionOutput::None,
                metrics,
                responses,
                errors: vec![error.execution_error(None)],
                recoveries: active_recoveries(),
            },
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
        let request = serde_json::from_str::<ExecutionRequest>(request_json)
            .map_err(|error| EngineError::InvalidInput(error.to_string()))?;
        let control = ExecutionControl::new(cancellation)
            .with_optional_deadline(request.options.deadline.as_deref())?;
        let result = self.execute_with_control(request, control).await;
        serde_json::to_string(&result).map_err(|error| EngineError::Runtime(error.to_string()))
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
        let (prepared, is_poll_result, active_key) = match &request.connection.polling {
            Some(polling) => {
                let completion = if polling.resume.is_some() {
                    self.await_resumed_poll_completion(
                        &request.connection,
                        polling,
                        &initial.url,
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
            return Err(EngineError::FileIo(format!(
                "upload source '{}' is not a regular file",
                source.display()
            )));
        }
        let length = metadata.len();
        if length > limit {
            return Err(EngineError::FileTooLarge { limit_bytes: limit });
        }
        let sha256 = hash_file(&source, limit).await?;
        if let Some(expected) = validated_checksum(file.expected_sha256.as_deref())? {
            if !sha256.eq_ignore_ascii_case(&expected) {
                return Err(EngineError::ChecksumMismatch {
                    expected,
                    actual: sha256,
                });
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
                validate_multipart_name(&file.field_name, "field_name")?;
                let filename = file
                    .filename
                    .clone()
                    .or_else(|| {
                        source
                            .file_name()
                            .map(|value| value.to_string_lossy().into_owned())
                    })
                    .ok_or_else(|| {
                        EngineError::InvalidInput(
                            "upload file requires an explicit filename".to_owned(),
                        )
                    })?;
                validate_multipart_name(&filename, "filename")?;
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
                return Err(EngineError::InvalidInput(
                    "upload requires request.body_type 'raw' or 'multipart'".to_owned(),
                ));
            }
        }
        let (value, _, _, _) = self
            .request_prepared_json(
                &request.connection,
                prepared,
                metrics,
                responses,
                &request.options,
            )
            .await?;
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
                self.paginated(
                    &request.connection,
                    &parameters,
                    pagination,
                    metrics,
                    responses,
                    &request.options,
                )
                .await?
            }
        };
        let records = map_records(values, &request.connection.response);
        let succeeded = records.len();
        Ok(OperationResult {
            output: ExecutionOutput::Records { records },
            errors: Vec::new(),
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
            return Err(EngineError::InvalidInput(
                "enrichment_concurrency must be greater than zero".to_owned(),
            ));
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
                            .map(|values| map_records(values, &request.connection.response))
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
                            response_records(&request.connection, &value)
                                .map(|values| map_records(values, &request.connection.response))
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

        let mut ordered: Vec<Option<EnrichmentOutcome>> =
            (0..request.input.records.len()).map(|_| None).collect();
        for outcome in outcomes {
            let index = outcome.index;
            ordered[index] = Some(outcome);
        }

        let mut output = Vec::with_capacity(request.input.records.len());
        let mut errors = Vec::new();
        let mut succeeded = 0_usize;
        for outcome in ordered.into_iter().flatten() {
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
            return Err(EngineError::InvalidInput(
                "batch max_size must be greater than zero".to_owned(),
            ));
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
                match resolve_parameters(&connection, &source) {
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
                Ok(mapped) => {
                    let error = EngineError::InvalidResponse(format!(
                        "batch returned {} results for {} input records",
                        mapped.len(),
                        valid.len()
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
    ) -> Result<Vec<Value>, EngineError> {
        let mut output = Vec::new();

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
                        .request_json(
                            connection,
                            &parameters,
                            None,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    let page = response_records(connection, &value)?;
                    let page_len = page.len();
                    append_limited(&mut output, page, *max_rows);
                    if page_len < limit {
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
                    let limit = (*page_size).min(max_rows.saturating_sub(output.len()));
                    let mut parameters = base_parameters.clone();
                    parameters.insert(page_param.clone(), usize_value(page_number)?);
                    parameters.insert(page_size_param.clone(), usize_value(limit)?);
                    let scoped_options = scoped_execution_options(options, request_index);
                    let (value, _, _, _) = self
                        .request_json(
                            connection,
                            &parameters,
                            None,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    let page = response_records(connection, &value)?;
                    let page_len = page.len();
                    append_limited(&mut output, page, *max_rows);
                    if page_len < limit {
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
                        .request_json(
                            connection,
                            &parameters,
                            None,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    cursor = json_path::get(&value, cursor_path).and_then(value_as_string);
                    match &cursor {
                        Some(value) if seen.insert(value.clone()) => {}
                        _ => break,
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
                        .request_json(
                            connection,
                            parameters,
                            next_url.as_deref(),
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    let Some(link) = json_path::get(&value, link_path).and_then(Value::as_str)
                    else {
                        break;
                    };
                    let resolved =
                        pagination_url(&final_url, link, *allow_cross_origin)?.to_string();
                    if !seen.insert(resolved.clone()) {
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
                    return Err(EngineError::InvalidInput(
                        "pagination Link relation cannot be empty".to_owned(),
                    ));
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
                        .request_json(
                            connection,
                            parameters,
                            next_url.as_deref(),
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    let Some(link) = link_header_target(&headers, relation)? else {
                        break;
                    };
                    let resolved =
                        pagination_url(&final_url, &link, *allow_cross_origin)?.to_string();
                    if !seen.insert(resolved.clone()) {
                        break;
                    }
                    next_url = Some(resolved);
                }
            }
        }

        Ok(output)
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
        self.request_prepared_json(connection, request, metrics, responses, options)
            .await
    }

    async fn request_prepared_json(
        &self,
        connection: &ConnectionConfig,
        request: PreparedRequest,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>), EngineError> {
        let result = match &connection.polling {
            Some(polling) if polling.resume.is_some() => {
                self.resume_poll(connection, polling, &request.url, metrics)
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
        metrics: &mut ExecutionMetrics,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>, u16), EngineError> {
        let completion = self
            .await_poll_completion(
                connection,
                polling,
                initial_value,
                initial_response,
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
        base_url: &Url,
        metrics: &mut ExecutionMetrics,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>, u16), EngineError> {
        let completion = self
            .await_resumed_poll_completion(connection, polling, base_url, metrics)
            .await?;
        let active_key = completion.active_key.clone();
        let result = self
            .complete_poll(
                connection,
                polling,
                completion.value,
                completion.response,
                completion.job_id.as_ref(),
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
            Some(PollState::Failure(status)) => {
                return Err(EngineError::InvalidResponse(format!(
                    "asynchronous operation failed with status '{status}'"
                )));
            }
            Some(PollState::Pending) | None => {}
        }
        let poll_url = poll_url(&initial_value, &initial_response, polling)?;
        if !polling.allow_cross_origin && !same_origin(&initial_response.final_url, &poll_url) {
            return Err(EngineError::UnsafeAddress(
                "cross-origin polling is blocked".to_owned(),
            ));
        }

        let active_key =
            self.register_polled_job(connection, polling, &poll_url, job_id.as_ref())?;
        self.await_poll_url(connection, polling, poll_url, job_id, active_key, metrics)
            .await
    }

    async fn await_resumed_poll_completion(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        base_url: &Url,
        metrics: &mut ExecutionMetrics,
    ) -> Result<PollCompletion, EngineError> {
        let resume = polling.resume.as_ref().ok_or_else(|| {
            EngineError::InvalidInput("polling resume configuration is missing".to_owned())
        })?;
        validate_job_id(&resume.job_id)?;
        let job_id = Value::String(resume.job_id.clone());
        let template = polling.url_template.as_deref().ok_or_else(|| {
            EngineError::InvalidInput(
                "polling resume requires url_template and cannot infer a prior Location URL"
                    .to_owned(),
            )
        })?;
        let target = render_poll_template_from_base(template, base_url, Some(&job_id))?;
        let poll_url = base_url
            .join(&target)
            .map_err(|error| EngineError::InvalidUrl(format!("polling resume URL: {error}")))?;
        if !polling.allow_cross_origin && !same_origin(base_url, &poll_url) {
            return Err(EngineError::UnsafeAddress(
                "cross-origin polling is blocked".to_owned(),
            ));
        }
        let active_key = self.register_polled_job(connection, polling, &poll_url, Some(&job_id))?;
        self.await_poll_url(
            connection,
            polling,
            poll_url,
            Some(job_id),
            active_key,
            metrics,
        )
        .await
    }

    async fn await_poll_url(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        poll_url: Url,
        job_id: Option<Value>,
        active_key: String,
        metrics: &mut ExecutionMetrics,
    ) -> Result<PollCompletion, EngineError> {
        if polling.max_attempts == 0 {
            remove_active_job(&active_key);
            return Err(EngineError::InvalidInput(
                "polling max_attempts must be greater than zero".to_owned(),
            ));
        }
        let poll_started = Instant::now();
        let mut interval_ms = polling.interval_ms;
        for _ in 0..polling.max_attempts {
            if polling
                .max_wait_ms
                .is_some_and(|limit| poll_started.elapsed().as_millis() >= u128::from(limit))
            {
                break;
            }
            if interval_ms > 0 {
                let delay = polling.max_wait_ms.map_or(interval_ms, |limit| {
                    interval_ms.min(limit.saturating_sub(
                        poll_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    ))
                });
                sleep(Duration::from_millis(delay)).await;
            }
            let request = self.prepare_followup_request(
                connection,
                poll_url.clone(),
                polling.method.clone(),
                connection
                    .request
                    .timeout_ms
                    .unwrap_or(self.config.request_timeout_ms),
                CachePolicy::default(),
            );
            let (value, response) = self
                .execute_prepared_json(connection, request, metrics, true)
                .await?;
            match poll_state(&value, polling)? {
                Some(PollState::Success) => {
                    return Ok(PollCompletion {
                        value,
                        response,
                        job_id,
                        active_key: Some(active_key),
                    });
                }
                Some(PollState::Failure(status)) => {
                    remove_active_job(&active_key);
                    return Err(EngineError::InvalidResponse(format!(
                        "asynchronous operation failed with status '{status}'"
                    )));
                }
                Some(PollState::Pending) => {}
                None => {
                    return Err(EngineError::InvalidResponse(format!(
                        "poll response has no status at '{}'",
                        polling.status_path
                    )));
                }
            }
            interval_ms = ((interval_ms as f64 * polling.interval_backoff.max(1.0))
                .min(polling.max_interval_ms as f64)) as u64;
        }

        self.cancel_active_job(&active_key, RemoteCancelTrigger::PollTimeout)
            .await;
        Err(EngineError::PollingTimeout {
            attempts: polling.max_attempts,
        })
    }

    fn register_polled_job(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        poll_url: &Url,
        job_id: Option<&Value>,
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
            .map(|cancel| self.prepare_remote_cancel(connection, polling, cancel, poll_url, job_id))
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
    ) -> Result<ActiveRemoteCancel, EngineError> {
        if cancel.timeout_ms == 0 {
            return Err(EngineError::InvalidInput(
                "polling cancel timeout_ms must be greater than zero".to_owned(),
            ));
        }
        let target = match cancel.url_template.as_deref() {
            Some(template) => {
                let rendered = render_poll_template_from_base(template, poll_url, job_id)?;
                poll_url.join(&rendered).map_err(|error| {
                    EngineError::InvalidUrl(format!("polling cancel URL: {error}"))
                })?
            }
            None => poll_url.clone(),
        };
        if !polling.allow_cross_origin && !same_origin(poll_url, &target) {
            return Err(EngineError::UnsafeAddress(
                "cross-origin polling cancellation is blocked".to_owned(),
            ));
        }
        let mut request = self.prepare_followup_request(
            connection,
            target,
            cancel.method.clone(),
            cancel.timeout_ms,
            CachePolicy::default(),
        );
        request.retry.max_attempts = 1;
        Ok(ActiveRemoteCancel {
            request,
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
            cache,
            circuit_breaker: connection.circuit_breaker.clone(),
            requests_per_second: connection.requests_per_second,
            tls: connection.tls.clone(),
            proxy: connection.proxy.clone(),
        }
    }

    async fn complete_poll(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        status_value: Value,
        status_response: ResponseData,
        job_id: Option<&Value>,
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
    ) -> Result<PreparedRequest, EngineError> {
        let target = polling_result_url(status_value, status_response, polling, job_id)?;
        if !polling.allow_cross_origin && !same_origin(&status_response.final_url, &target) {
            return Err(EngineError::UnsafeAddress(
                "cross-origin polling result URL is blocked".to_owned(),
            ));
        }
        Ok(PreparedRequest {
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
            cache: connection.cache.clone(),
            circuit_breaker: connection.circuit_breaker.clone(),
            requests_per_second: connection.requests_per_second,
            tls: connection.tls.clone(),
            proxy: connection.proxy.clone(),
        })
    }

    async fn resolve_upload_source(
        &self,
        file: &FileTransferInput,
    ) -> Result<PathBuf, EngineError> {
        self.ensure_file_transfers_allowed()?;
        let root = self.canonical_file_root().await?;
        let base = match &root {
            Some(root) => root.clone(),
            None => canonical_current_directory().await?,
        };
        let requested = required_path(&file.path)?;
        let candidate = if requested.is_absolute() {
            requested
        } else {
            base.join(requested)
        };
        let source = fs::canonicalize(candidate).await.map_err(file_io)?;
        ensure_within_root(&source, root.as_deref())?;
        Ok(source)
    }

    async fn resolve_download_target(
        &self,
        file: &FileTransferInput,
    ) -> Result<PathBuf, EngineError> {
        self.ensure_file_transfers_allowed()?;
        let root = self.canonical_file_root().await?;
        let base = match &root {
            Some(root) => root.clone(),
            None => canonical_current_directory().await?,
        };
        let requested = required_path(&file.path)?;
        let candidate = if requested.is_absolute() {
            requested
        } else {
            base.join(requested)
        };
        let filename = candidate.file_name().ok_or_else(|| {
            EngineError::InvalidInput("download path must include a filename".to_owned())
        })?;
        let parent = candidate.parent().ok_or_else(|| {
            EngineError::InvalidInput("download path has no parent directory".to_owned())
        })?;
        let parent = fs::canonicalize(parent).await.map_err(file_io)?;
        ensure_within_root(&parent, root.as_deref())?;
        let target = parent.join(filename);
        if fs::try_exists(&target).await.map_err(file_io)? {
            let metadata = fs::symlink_metadata(&target).await.map_err(file_io)?;
            if metadata.is_dir() {
                return Err(EngineError::FileIo(format!(
                    "download destination '{}' is a directory",
                    target.display()
                )));
            }
            if !file.overwrite {
                return Err(EngineError::FileIo(format!(
                    "download destination '{}' already exists",
                    target.display()
                )));
            }
        }
        Ok(target)
    }

    fn ensure_file_transfers_allowed(&self) -> Result<(), EngineError> {
        if self.config.allow_file_transfers {
            Ok(())
        } else {
            Err(EngineError::PolicyViolation(
                "file transfers are not enabled for this engine".to_owned(),
            ))
        }
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
            return Err(EngineError::FileIo(format!(
                "configured file_root '{}' is not a directory",
                root.display()
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
            return Err(EngineError::InvalidUrl("URL is empty".to_owned()));
        }
        let (rendered_url, consumed) = render_template(template, parameters, true);
        let mut url = Url::parse(&rendered_url)
            .map_err(|error| EngineError::InvalidUrl(error.to_string()))?;
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
                    return Err(EngineError::InvalidInput(format!(
                        "parameter '{name}' is used in the URL but has location '{location:?}'"
                    )));
                }
                continue;
            }
            match location {
                ParameterLocation::Path => {
                    return Err(EngineError::InvalidInput(format!(
                        "path parameter '{name}' has no matching URL placeholder"
                    )));
                }
                ParameterLocation::Query => {
                    query_parameters.insert(name.clone(), value.clone());
                }
                ParameterLocation::Header => {
                    insert_header(&mut headers, name, parameter_header_value(value));
                }
                ParameterLocation::Body => {
                    body_parameters.insert(name.clone(), value.clone());
                }
                ParameterLocation::Cookie => {
                    validate_cookie_name(name)?;
                    cookies.push((name.clone(), cookie_value(value)?));
                }
                ParameterLocation::Auto if legacy_query_only => {
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
                BodyType::FormUrlencoded => PreparedBody::Form(
                    body_parameters
                        .iter()
                        .map(|(key, value)| (key.clone(), value_as_text(value)))
                        .collect(),
                ),
                BodyType::Multipart => {
                    multipart_body(&body_parameters, self.config.max_request_bytes)?
                }
                BodyType::Raw => {
                    let raw = connection.request.raw_body.as_deref().unwrap_or_default();
                    let (rendered, _) = render_template(raw, parameters, false);
                    ensure_request_size(rendered.len(), self.config.max_request_bytes)?;
                    PreparedBody::Raw(rendered)
                }
                BodyType::None => PreparedBody::None,
            }
        };
        match &body {
            PreparedBody::Json(value) => {
                let size = serde_json::to_vec(value)
                    .map_err(|error| EngineError::InvalidInput(error.to_string()))?
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
    request
        .input
        .file
        .as_ref()
        .ok_or_else(|| EngineError::InvalidInput("operation requires input.file".to_owned()))
}

fn required_path(value: &str) -> Result<PathBuf, EngineError> {
    if value.trim().is_empty() {
        Err(EngineError::InvalidInput(
            "file path cannot be empty".to_owned(),
        ))
    } else {
        Ok(PathBuf::from(value))
    }
}

fn transfer_limit(config: &EngineConfig, file: &FileTransferInput) -> Result<u64, EngineError> {
    if config.max_file_transfer_bytes == 0 {
        return Err(EngineError::PolicyViolation(
            "max_file_transfer_bytes must be greater than zero".to_owned(),
        ));
    }
    match file.max_bytes {
        Some(0) => Err(EngineError::InvalidInput(
            "input.file.max_bytes must be greater than zero".to_owned(),
        )),
        Some(limit) => Ok(limit.min(config.max_file_transfer_bytes)),
        None => Ok(config.max_file_transfer_bytes),
    }
}

fn validated_checksum(value: Option<&str>) -> Result<Option<String>, EngineError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(EngineError::InvalidInput(
            "expected_sha256 must contain exactly 64 hexadecimal characters".to_owned(),
        ));
    }
    Ok(Some(value.to_ascii_lowercase()))
}

fn ensure_within_root(path: &Path, root: Option<&Path>) -> Result<(), EngineError> {
    if root.is_none_or(|root| path.starts_with(root)) {
        Ok(())
    } else {
        Err(EngineError::PolicyViolation(format!(
            "file path '{}' is outside the configured file_root",
            path.display()
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
        let read = u64::try_from(read)
            .map_err(|_| EngineError::Runtime("file read length overflowed u64".to_owned()))?;
        if total.saturating_add(read) > limit {
            return Err(EngineError::FileTooLarge { limit_bytes: limit });
        }
        digest.update(
            &buffer[..usize::try_from(read).map_err(|_| {
                EngineError::Runtime("file read length overflowed usize".to_owned())
            })?],
        );
        total = total.saturating_add(read);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_multipart_name(value: &str, name: &str) -> Result<(), EngineError> {
    if value.trim().is_empty() || value.contains(['\r', '\n', '\0']) {
        Err(EngineError::InvalidInput(format!(
            "upload {name} is empty or contains unsupported characters"
        )))
    } else {
        Ok(())
    }
}

fn file_io(error: std::io::Error) -> EngineError {
    EngineError::FileIo(error.to_string())
}

enum PollState {
    Pending,
    Success,
    Failure(String),
}

fn poll_state(response: &Value, polling: &PollingConfig) -> Result<Option<PollState>, EngineError> {
    let Some(value) = json_path::get(response, &polling.status_path) else {
        return Ok(None);
    };
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
        return Ok(Some(PollState::Failure(status)));
    }
    Err(EngineError::InvalidResponse(format!(
        "unknown asynchronous status '{status}'"
    )))
}

fn evaluate_application_success(
    response: &Value,
    connection: &ConnectionConfig,
) -> Result<(), EngineError> {
    if let Some(path) = &connection.response.error_path {
        if let Some(value) = json_path::get(response, path)
            .filter(|value| !value.is_null() && value.as_str() != Some(""))
        {
            return Err(EngineError::Application(application_error_text(value)));
        }
    }
    if let Some(condition) = &connection.response.success_when {
        if !matches_success_condition(response, condition) {
            return Err(EngineError::Application(
                "success_when condition was not satisfied".to_owned(),
            ));
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

fn application_error_text(value: &Value) -> String {
    value
        .as_object()
        .and_then(|object| {
            ["detail", "error", "message"]
                .into_iter()
                .find_map(|key| object.get(key))
        })
        .map(value_as_text)
        .unwrap_or_else(|| value_as_text(value))
}

fn poll_result(response: Value, polling: &PollingConfig) -> Result<Value, EngineError> {
    match &polling.result_path {
        Some(path) => json_path::get(&response, path).cloned().ok_or_else(|| {
            EngineError::InvalidResponse(format!("polling result path '{path}' was not found"))
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
                EngineError::InvalidResponse(format!("polling URL path '{path}' was not found"))
            })?
            .to_owned()
    } else if let Some(template) = &polling.url_template {
        let job_id = polling_job_id(response, response_data, polling).ok_or_else(|| {
            let source = polling.id_header.as_deref().unwrap_or(&polling.id_path);
            EngineError::InvalidResponse(format!("polling job id '{source}' was not found"))
        })?;
        render_poll_template(template, response_data, Some(&job_id))?
    } else if let Some(header) = &polling.location_header {
        response_data
            .headers
            .get(&header.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| {
                EngineError::InvalidResponse(format!("polling response has no '{header}' header"))
            })?
    } else {
        return Err(EngineError::InvalidInput(
            "polling requires url_path, url_template, or location_header".to_owned(),
        ));
    };

    response_data
        .final_url
        .join(&target)
        .map_err(|error| EngineError::InvalidUrl(format!("polling URL: {error}")))
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
        json_path::get(response, &polling.id_path).cloned()
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
            EngineError::InvalidInput("polling result URL is not configured".to_owned())
        })?;
        let template = json_path::get(response, path)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                EngineError::InvalidResponse(format!(
                    "polling result URL path '{path}' was not found"
                ))
            })?;
        render_poll_template(template, response_data, job_id)?
    };
    response_data
        .final_url
        .join(&target)
        .map_err(|error| EngineError::InvalidUrl(format!("polling result URL: {error}")))
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
        EngineError::InvalidResponse("polling result URL requires a job id".to_owned())
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
        return Err(EngineError::InvalidInput(
            "polling job_id must contain 1 to 512 non-control characters".to_owned(),
        ));
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
                return Err(EngineError::MissingParameter(parameter.name.clone()));
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
            EngineError::InvalidResponse(format!("records path '{path}' was not found"))
        })?,
        None => response,
    };
    if connection.response.iterate_on.is_empty() {
        return match selected {
            Value::Array(values) => Ok(values.clone()),
            Value::Object(_) => Ok(vec![selected.clone()]),
            Value::Null => Ok(Vec::new()),
            _ => Err(EngineError::InvalidResponse(
                "records value must be an array, object, or null".to_owned(),
            )),
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
        BatchInputFormat::FlatArray => Value::Array(
            parameters
                .into_iter()
                .filter_map(|parameters| parameters.into_values().find(|value| !value.is_null()))
                .collect(),
        ),
        BatchInputFormat::Array | BatchInputFormat::Object => {
            Value::Array(parameters.into_iter().map(Value::Object).collect())
        }
    };
    Map::from_iter([(batch.input_key.clone(), values)])
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
            EngineError::InvalidResponse(format!(
                "batch output path '{}' was not found",
                batch.output_path
            ))
        })?
    };
    let values = selected
        .as_array()
        .ok_or_else(|| EngineError::InvalidResponse("batch output must be an array".to_owned()))?;
    Ok(values
        .iter()
        .map(|value| {
            if value.is_null() {
                return JsonObject::new();
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
            apply_transforms(&mut row, &config.transforms);
            row
        })
        .collect())
}

fn expand_iterations(
    current: &Value,
    iterations: &[crate::IterationSpec],
    level: usize,
    context: &JsonObject,
    output: &mut Vec<Value>,
) {
    if level >= iterations.len() {
        output.push(Value::Object(context.clone()));
        return;
    }
    let iteration = &iterations[level];
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

fn map_records(values: Vec<Value>, response: &ResponseConfig) -> Vec<JsonObject> {
    values
        .iter()
        .map(|value| {
            let mut row = map_value(value, &response.output_mapping);
            apply_transforms(&mut row, &response.transforms);
            row
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

fn apply_transforms(row: &mut JsonObject, transforms: &[ResponseTransform]) {
    for transform in transforms {
        if transform.column.is_empty()
            || transform
                .condition
                .as_deref()
                .is_some_and(|condition| !transform_condition(row, condition))
        {
            continue;
        }
        let source = row.get(&transform.source).cloned().unwrap_or(Value::Null);
        let value = transform_value(&source, transform);
        row.insert(transform.column.clone(), value);
    }
}

fn transform_condition(row: &JsonObject, condition: &str) -> bool {
    let (left, expected, equals) = if let Some((left, right)) = condition.split_once("==") {
        (left, right, true)
    } else if let Some((left, right)) = condition.split_once("!=") {
        (left, right, false)
    } else {
        return true;
    };
    let actual = row.get(left.trim()).map(value_as_text).unwrap_or_default();
    let expected = expected
        .trim()
        .trim_matches(|character| character == '\'' || character == '"');
    (actual == expected) == equals
}

fn transform_value(source: &Value, transform: &ResponseTransform) -> Value {
    let original = || source.clone();
    let numeric = || source.as_f64().or_else(|| source.as_str()?.parse().ok());
    let argument = || {
        transform
            .value
            .as_ref()
            .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
    };
    let number = |value: f64| {
        Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    };
    match transform.operation.as_str() {
        "subtract" => numeric()
            .zip(argument())
            .map(|(a, b)| number(a - b))
            .unwrap_or_else(original),
        "add" => numeric()
            .zip(argument())
            .map(|(a, b)| number(a + b))
            .unwrap_or_else(original),
        "multiply" => numeric()
            .zip(argument())
            .map(|(a, b)| number(a * b))
            .unwrap_or_else(original),
        "divide" => numeric().zip(argument()).map_or_else(original, |(a, b)| {
            if b == 0.0 { Value::Null } else { number(a / b) }
        }),
        "round" => numeric().map_or_else(original, |value| {
            let decimals = argument().unwrap_or(0.0).clamp(0.0, 15.0) as i32;
            let factor = 10_f64.powi(decimals);
            number((value * factor).round() / factor)
        }),
        "kelvin_to_celsius" => numeric()
            .map(|value| number(value - 273.15))
            .unwrap_or_else(original),
        "celsius_to_kelvin" => numeric()
            .map(|value| number(value + 273.15))
            .unwrap_or_else(original),
        "uppercase" => source
            .as_str()
            .map(|value| Value::String(value.to_uppercase()))
            .unwrap_or(Value::Null),
        "lowercase" => source
            .as_str()
            .map(|value| Value::String(value.to_lowercase()))
            .unwrap_or(Value::Null),
        "prefix" => transform
            .value
            .as_ref()
            .map(|prefix| {
                Value::String(format!(
                    "{}{}",
                    value_as_text(prefix),
                    value_as_text(source)
                ))
            })
            .unwrap_or(Value::Null),
        "suffix" => transform
            .value
            .as_ref()
            .map(|suffix| {
                Value::String(format!(
                    "{}{}",
                    value_as_text(source),
                    value_as_text(suffix)
                ))
            })
            .unwrap_or(Value::Null),
        "replace" => transform
            .value
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|value| {
                let find = value.get("find")?.as_str()?;
                let replacement = value.get("replace")?.as_str()?;
                Some(Value::String(
                    value_as_text(source).replace(find, replacement),
                ))
            })
            .unwrap_or(Value::Null),
        "default_if_null" if source.is_null() => transform.value.clone().unwrap_or(Value::Null),
        "default_if_null" => source.clone(),
        _ => source.clone(),
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
            return Err(EngineError::InvalidInput(format!(
                "deep_object query parameter '{name}' must be an object"
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
        return Err(EngineError::InvalidInput(
            "cookie parameter contains unsupported characters".to_owned(),
        ));
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
        Err(EngineError::InvalidInput(format!(
            "invalid cookie parameter name '{name}'"
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
                    EngineError::InvalidInput(format!(
                        "multipart file '{name}' has invalid data_base64"
                    ))
                })?;
            let data = STANDARD.decode(encoded).map_err(|_| {
                EngineError::InvalidInput(format!("multipart file '{name}' is not valid base64"))
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
                        EngineError::InvalidInput(format!(
                            "multipart file '{name}' has invalid content_type"
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
        .map_err(|_| EngineError::InvalidInput("numeric value is too large".to_owned()))?;
    Ok(Value::Number(Number::from(value)))
}

fn ensure_page_size(page_size: usize) -> Result<(), EngineError> {
    if page_size == 0 {
        Err(EngineError::InvalidInput(
            "pagination page_size must be greater than zero".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn append_limited(output: &mut Vec<Value>, values: Vec<Value>, max_rows: usize) {
    let remaining = max_rows.saturating_sub(output.len());
    output.extend(values.into_iter().take(remaining));
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
    let resolved = base.join(target).map_err(|error| {
        EngineError::InvalidResponse(format!("invalid pagination link: {error}"))
    })?;
    if !allow_cross_origin && !same_origin(base, &resolved) {
        return Err(EngineError::UnsafeAddress(
            "cross-origin pagination is blocked".to_owned(),
        ));
    }
    Ok(resolved)
}

fn link_header_target(
    headers: &BTreeMap<String, String>,
    expected_relation: &str,
) -> Result<Option<String>, EngineError> {
    let Some(header) = headers.get("link") else {
        return Ok(None);
    };
    for entry in split_link_header(header)? {
        let entry = entry.trim();
        let Some(target_end) = entry.strip_prefix('<').and_then(|value| value.find('>')) else {
            return Err(EngineError::InvalidResponse(
                "Link header contains an invalid target".to_owned(),
            ));
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
        return Err(EngineError::InvalidResponse(
            "Link header is not well formed".to_owned(),
        ));
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
        return Err(EngineError::InvalidResponse(
            "Link header parameter is not well formed".to_owned(),
        ));
    }
    values.push(&value[start..]);
    Ok(values)
}

fn unquote_header_value(value: &str) -> Result<&str, EngineError> {
    match (value.strip_prefix('"'), value.strip_suffix('"')) {
        (Some(value), Some(_)) if value.ends_with('"') => Ok(&value[..value.len() - 1]),
        (None, None) => Ok(value),
        _ => Err(EngineError::InvalidResponse(
            "Link header contains an invalid quoted value".to_owned(),
        )),
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
            !is_sensitive_response_header(name)
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
        return Err(EngineError::InvalidInput(
            "idempotency field name must contain 1 to 256 non-control characters".to_owned(),
        ));
    }
    match connection.idempotency.location {
        IdempotencyLocation::Header => {
            if let Some(existing) = headers
                .iter()
                .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
            {
                if existing != key {
                    return Err(EngineError::InvalidInput(
                        "idempotency header conflicts with a configured header".to_owned(),
                    ));
                }
            }
            insert_header(headers, name, key.to_owned());
        }
        IdempotencyLocation::Query => {
            insert_idempotency_field(query, name, key, "query")?;
        }
        IdempotencyLocation::Body => {
            if matches!(connection.request.body_type, BodyType::None | BodyType::Raw) {
                return Err(EngineError::InvalidInput(
                    "body idempotency requires json, form_urlencoded, or multipart body".to_owned(),
                ));
            }
            insert_idempotency_field(body, name, key, "body")?;
        }
    }
    Ok(())
}

fn insert_idempotency_field(
    target: &mut JsonObject,
    name: &str,
    key: &str,
    location: &str,
) -> Result<(), EngineError> {
    if let Some(existing) = target.get(name) {
        if existing.as_str() != Some(key) {
            return Err(EngineError::InvalidInput(format!(
                "idempotency {location} field conflicts with a configured parameter"
            )));
        }
    }
    target.insert(name.to_owned(), Value::String(key.to_owned()));
    Ok(())
}

fn public_response_url(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn is_sensitive_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "cookie"
            | "set-cookie"
            | "www-authenticate"
            | "x-api-key"
    )
}

fn failed_result(error: EngineError) -> ExecutionResult {
    failed_result_with_recoveries(error, Vec::new())
}

fn failed_result_with_recoveries(
    error: EngineError,
    recoveries: Vec<AsyncJobRecovery>,
) -> ExecutionResult {
    ExecutionResult {
        schema_version: SCHEMA_VERSION,
        status: ExecutionStatus::Failed,
        output: ExecutionOutput::None,
        metrics: ExecutionMetrics::default(),
        responses: Vec::new(),
        errors: vec![error.execution_error(None)],
        recoveries,
    }
}

fn validate_idempotency_key(key: &str) -> Result<(), EngineError> {
    if key.is_empty() || key.len() > 255 || !key.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err(EngineError::InvalidInput(
            "idempotency_key must contain 1 to 255 visible ASCII bytes".to_owned(),
        ));
    }
    Ok(())
}

fn validate_execution_configuration(request: &ExecutionRequest) -> Result<(), EngineError> {
    let Some(polling) = request.connection.polling.as_ref() else {
        return Ok(());
    };
    if polling.max_attempts == 0 {
        return Err(EngineError::InvalidInput(
            "polling max_attempts must be greater than zero".to_owned(),
        ));
    }
    if polling.max_wait_ms == Some(0) {
        return Err(EngineError::InvalidInput(
            "polling max_wait_ms must be greater than zero when configured".to_owned(),
        ));
    }
    if !polling.interval_backoff.is_finite() || polling.interval_backoff < 1.0 {
        return Err(EngineError::InvalidInput(
            "polling interval_backoff must be finite and at least one".to_owned(),
        ));
    }
    if polling
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.timeout_ms == 0)
    {
        return Err(EngineError::InvalidInput(
            "polling cancel timeout_ms must be greater than zero".to_owned(),
        ));
    }

    let Some(resume) = polling.resume.as_ref() else {
        return Ok(());
    };
    validate_job_id(&resume.job_id)?;
    if polling.url_template.is_none() {
        return Err(EngineError::InvalidInput(
            "polling resume requires url_template and cannot infer a prior Location URL".to_owned(),
        ));
    }
    if request.connection.pagination.is_some() {
        return Err(EngineError::InvalidInput(
            "polling resume cannot be combined with pagination".to_owned(),
        ));
    }
    if request
        .connection
        .batch
        .as_ref()
        .is_some_and(|batch| batch.enabled)
    {
        return Err(EngineError::InvalidInput(
            "polling resume cannot be combined with batch execution".to_owned(),
        ));
    }
    if request.operation == ExecutionOperation::Enrich && request.input.records.len() != 1 {
        return Err(EngineError::InvalidInput(
            "polling resume for enrichment requires exactly one input record".to_owned(),
        ));
    }
    Ok(())
}

fn execution_fingerprint(request: &ExecutionRequest) -> Result<String, EngineError> {
    let mut normalized = request.clone();
    normalized.options.deadline = None;
    normalized.options.idempotency_key = None;
    let bytes =
        serde_json::to_vec(&normalized).map_err(|error| EngineError::Runtime(error.to_string()))?;
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

fn active_recoveries() -> Vec<AsyncJobRecovery> {
    active_jobs_handle()
        .map(|jobs| recoveries_from(&jobs))
        .unwrap_or_default()
}

fn recoveries_from(jobs: &Arc<Mutex<BTreeMap<String, ActiveAsyncJob>>>) -> Vec<AsyncJobRecovery> {
    jobs.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter_map(|job| job.recovery.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{
        link_header_target, map_value, render_template, resolve_parameters,
        selected_response_headers,
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
}
