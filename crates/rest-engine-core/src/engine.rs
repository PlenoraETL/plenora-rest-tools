#[cfg(test)]
use credentials::names_an_idempotency_key;
#[cfg(test)]
use mapping::map_value;
mod polling;
use polling::{
    RecoveryHandles, active_recoveries, evaluate_application_success, recoveries_from,
    remove_active_job, validate_job_id,
};
mod artifacts;
mod pagination;
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) use pagination::link_header_target;
mod responses;
use responses::selected_response_headers;
mod request;
use request::render_template;
mod mapping;
use mapping::{
    batch_payload, batch_response, flat_batch_item, map_records, resolve_parameters,
    response_records, usize_value, validate_transforms, value_as_string, value_as_text,
};
mod credentials;
use credentials::{
    CredentialScope, apply_idempotency, idempotency_header_withheld, insert_header,
    preserved_idempotency_header, public_response_url,
};
pub(crate) use credentials::{is_disallowed_idempotency_header, is_sensitive_header_name};
mod validation;
use validation::{
    validate_engine_configuration, validate_execution_configuration, validate_idempotency_key,
};

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

use crate::{control::ControlOutcome, error::ErrorDetail};
use base64::engine::general_purpose::STANDARD;
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
        PreparedFileSource, PreparedRequest, PreparedStream, RECORD_SCOPE, ResponseData, Transport,
        same_origin,
    },
};

/// REST execution engine.
///
/// An `Engine` owns everything that outlives a single execution: the
/// [`EngineConfig`] policies, HTTP connection pools, cached OAuth tokens, the
/// HTTP cache, circuit breaker state, the request rate limiter, cookie
/// sessions and the registry of admitted idempotency keys. Executions take
/// `&self`, so one engine can serve concurrent executions from several tasks;
/// the engine-wide limits (`max_concurrent_requests`, `requests_per_second`)
/// apply across all of them.
///
/// Executions never panic and never return a Rust error: every failure is
/// reported in the [`ExecutionResult`] with status `failed`.
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

mod records;

impl Engine {
    /// Creates an engine with the given configuration. Nothing is validated
    /// or connected here: policies are enforced when an execution uses them.
    pub fn new(config: EngineConfig) -> Self {
        Self {
            transport: Transport::new(config.clone()),
            config,
            closed: AtomicBool::new(false),
            idempotency: Mutex::new(IdempotencyRegistry::default()),
        }
    }

    /// Runs one execution with no external cancellation.
    ///
    /// Identical to [`execute_with_control`](Self::execute_with_control)
    /// with a default control: only `options.deadline` bounds the execution.
    pub async fn execute(&self, request: ExecutionRequest) -> ExecutionResult {
        self.execute_with_control(request, ExecutionControl::default())
            .await
    }

    /// Runs one execution under the cancellation token of `control` and the
    /// earlier of its deadline and `options.deadline` (RFC 3339 in UTC; a
    /// non-zero offset or `-00:00` is `INVALID_INPUT`).
    ///
    /// Before any network activity the engine checks, in order: that it is
    /// not closed (`ENGINE_CLOSED`), the deadline spelling and the request
    /// configuration (`INVALID_INPUT`), the cookie session handle
    /// (`POLICY_VIOLATION`), the idempotency key (`INVALID_INPUT`,
    /// `POLICY_VIOLATION`, `IDEMPOTENCY_CONFLICT`) and whether the deadline has
    /// already passed (`DEADLINE_EXPIRED`: timeout, phase validate, no remote
    /// effect). A cancellation fails the execution with `CANCELLED` and a
    /// deadline reached during it with `TIMEOUT`; in both cases asynchronous
    /// jobs being polled are cancelled
    /// remotely when their polling configuration asks for it, within a shared
    /// budget of 5 seconds, and their recovery data is returned in
    /// `recoveries`.
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
        if let Err(error) = validate_engine_configuration(&self.config) {
            return failed_result(error);
        }
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
        if let Err(error) = control.check_admission() {
            return failed_result(error);
        }
        let active_jobs = Arc::new(Mutex::new(BTreeMap::new()));
        let tally = Arc::new(ExecutionTally::default());
        let execution = EXECUTION_TALLY.scope(
            tally.clone(),
            ACTIVE_ASYNC_JOBS.scope(active_jobs.clone(), self.execute_inner(request)),
        );
        // Keep the large operation future off the caller's stack when the
        // shared control race polls it (notably the Windows CLI main thread).
        let outcome = control.run(Box::pin(execution)).await;
        let mut result = match outcome {
            ControlOutcome::Finished(result) => result,
            ControlOutcome::Cancelled => {
                // The remote cancellation is a request of this execution too.
                EXECUTION_TALLY
                    .scope(
                        tally.clone(),
                        self.cancel_active_jobs(&active_jobs, RemoteCancelTrigger::Cancellation),
                    )
                    .await;
                failed_result_with_recoveries(EngineError::Cancelled, recoveries_from(&active_jobs))
            }
            ControlOutcome::Deadline => {
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
        account_for_sent_requests(&mut result, &tally);
        result
    }

    /// The capability document of this build; the same value as the free
    /// function [`capabilities`](crate::capabilities).
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

    /// Closes the engine. From now on new executions fail with
    /// `ENGINE_CLOSED` and cookie session calls return
    /// [`EngineError::EngineClosed`]; executions already admitted run to
    /// completion. Idempotent and irreversible.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Whether [`close`](Self::close) has been called.
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

    /// JSON form of [`execute`](Self::execute): parses a
    /// `plenora-rest-execution-request-v1` document and returns the
    /// serialized `plenora-rest-execution-result-v1` document.
    ///
    /// Only failures outside the execution are a Rust error: a closed engine
    /// ([`EngineError::EngineClosed`]), text that does not parse as the
    /// request contract or an invalid `options.deadline`
    /// ([`EngineError::InvalidInput`], with only the line and column of a
    /// parse failure), or a result that cannot be serialized
    /// ([`EngineError::Runtime`]). A failed execution is a successful call
    /// returning a result with status `failed`.
    pub async fn execute_json(&self, request_json: &str) -> Result<String, EngineError> {
        self.execute_json_with_cancellation(request_json, CancellationToken::new())
            .await
    }

    /// [`execute_json`](Self::execute_json) with an external cancellation
    /// token; the deadline still comes from `options.deadline`.
    pub async fn execute_json_with_cancellation(
        &self,
        request_json: &str,
        cancellation: CancellationToken,
    ) -> Result<String, EngineError> {
        if self.is_closed() {
            return Err(EngineError::EngineClosed);
        }
        crate::control_json::validate(request_json.as_bytes()).map_err(|error| {
            EngineError::InvalidInput(ErrorDetail::at(error.message, error.line, error.column))
        })?;
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
        records::execute(self, request, metrics, responses).await
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
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

/// ERR-014: an error raised after a request was sent cannot report that
/// nothing happened remotely. An error of the whole operation is judged on
/// every request the execution sent; an error of one input record (enrich) on
/// the requests sent for that record only, so a record whose own request never
/// left keeps `none` while others went out. Only requests actually sent count
/// ([`ExecutionTally`]), not responses served from the cache.
///
/// The phase is normalized first and on its own, also with no request sent
/// (an execution served entirely from the cache): a `validate` error raised
/// after the transport entered a phase takes that phase. The effect and the
/// retry change only when a request was sent; otherwise they stay `none` and
/// `never`, since nothing went out.
///
/// The phase follows the same split as the requests: a record error takes its
/// record's phase, an error of the whole execution the phase of the paths
/// outside any record. In enrich every request runs inside a record, so the
/// execution has no phase of its own: an execution error raised after the
/// records' requests (an internal failure, a cancellation, a deadline) keeps
/// the phase of its variant, and one whose variant says `validate` takes
/// `read` explicitly when requests were sent, never a phase left by whichever
/// record ran last.
fn account_for_sent_requests(result: &mut ExecutionResult, tally: &ExecutionTally) {
    for error in &mut result.errors {
        tally.failure_context(error.input_index).apply(error);
    }
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
/// The contract caps `recoveries` at [`polling::MAX_RECOVERIES`]. A handle left out is
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

fn execution_fingerprint(request: &ExecutionRequest) -> Result<String, EngineError> {
    let mut normalized = request.clone();
    normalized.options.deadline = None;
    normalized.options.idempotency_key = None;
    let bytes = serde_json::to_vec(&normalized).map_err(|_| {
        EngineError::Runtime(ErrorDetail::from(
            "request could not be serialized for its fingerprint",
        ))
    })?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
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
        format!(
            "plenora-{}",
            digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    });
    scoped
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
    fn scoped_idempotency_digest_keeps_its_lowercase_zero_padded_encoding() {
        let options = crate::ExecutionOptions {
            idempotency_key: Some("key".to_owned()),
            ..Default::default()
        };
        // Independent SHA-256 vector for the domain, key and index, including
        // both NUL separators. The expected bytes are not computed by sha2.
        assert_eq!(
            super::scoped_execution_options(&options, 7)
                .idempotency_key
                .as_deref(),
            Some("plenora-df7c0788629c00c65922b16471036a7697997de97e79e1193460292603959a7b")
        );
    }

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
