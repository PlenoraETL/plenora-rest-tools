use std::{collections::BTreeMap, fmt};

use serde::Serialize;
use serde_json::{Value, json};

use crate::ExecutionError;

/// Diagnostic carried by an [`EngineError`] variant.
///
/// Only engine-authored text can be stored: the type is built from a
/// `&'static str`, so a remote message, a response body excerpt, an address, a
/// domain, an `io::Error`, a parser message or a checksum of the caller's data
/// cannot enter an error at all, rather than being carried and then hidden.
/// The only other content is a position (line and column) for a response that
/// failed to parse, which locates the failure without quoting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorDetail {
    text: &'static str,
    position: Option<(u64, u64)>,
}

impl ErrorDetail {
    /// A parse failure at a one-based line and column of the response.
    pub(crate) fn at(text: &'static str, line: usize, column: usize) -> Self {
        let to_u64 = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
        Self {
            text,
            position: Some((to_u64(line), to_u64(column))),
        }
    }

    /// The engine-authored description.
    pub fn text(&self) -> &'static str {
        self.text
    }

    /// Line and column of a response parse failure, when there is one.
    pub fn position(&self) -> Option<(u64, u64)> {
        self.position
    }
}

/// The kind of an I/O failure, never its message: `io::Error`'s text can name
/// local paths and comes from the operating system.
pub(crate) fn io_detail(error: &std::io::Error) -> ErrorDetail {
    use std::io::ErrorKind;
    ErrorDetail::from(match error.kind() {
        ErrorKind::NotFound => "file or directory not found",
        ErrorKind::PermissionDenied => "permission denied",
        ErrorKind::AlreadyExists => "file already exists",
        ErrorKind::InvalidInput | ErrorKind::InvalidData => "invalid file data or argument",
        ErrorKind::UnexpectedEof => "unexpected end of file",
        ErrorKind::WriteZero | ErrorKind::StorageFull => "storage is full or not writable",
        ErrorKind::Interrupted => "file operation was interrupted",
        _ => "file operation failed",
    })
}

impl From<&'static str> for ErrorDetail {
    fn from(text: &'static str) -> Self {
        Self {
            text,
            position: None,
        }
    }
}

/// Every failure the engine reports.
///
/// Variants that describe a failure in words carry an opaque `ErrorDetail`
/// variants that carry numbers chosen by the engine or the protocol (a byte
/// limit, an HTTP status, a poll count, a contract version) keep them public.
/// `Display` is the static public message of the variant, the same text as
/// `payload().message`, so formatting an error never prints remote or local
/// data.
///
/// A detail is engine-authored text only; a `String` built from remote or
/// local data does not convert into one:
///
/// ```compile_fail
/// let remote = String::from("message chosen by the remote service");
/// let _ = plenora_rest_core::EngineError::Application(remote.into());
/// ```
#[derive(Debug)]
pub enum EngineError {
    /// `INVALID_INPUT`: the request, its JSON text or a runtime envelope does
    /// not satisfy the contract (unknown or missing field, value out of its
    /// declared range, incompatible combination of options). Raised before
    /// any network activity; category `invalid_configuration`, never retried.
    InvalidInput(ErrorDetail),
    /// `UNSUPPORTED_SCHEMA`: the `schema_version` of a request or runtime
    /// envelope is not the one this engine implements. Category
    /// `unsupported`; `details` carries `received_version` and
    /// `supported_version`.
    UnsupportedSchema {
        /// Version found in the input.
        received: u32,
        /// The only version this engine accepts.
        supported: u32,
    },
    /// `INVALID_URL`: a request, token, polling or redirect URL is empty,
    /// unparsable, not `http`/`https`, has no host or usable port, or embeds
    /// credentials. Category `invalid_configuration`.
    InvalidUrl(ErrorDetail),
    /// `UNSAFE_ADDRESS`: the destination resolved to a private, loopback or
    /// otherwise non-public address that `allow_private_networks` does not
    /// permit, or a redirect, pagination link or polling URL would leave the
    /// origin of the request. Category `authorization`; nothing is sent.
    UnsafeAddress(ErrorDetail),
    /// `POLICY_VIOLATION`: an engine security or resource policy refused the
    /// request before the network: a feature the [`EngineConfig`] does not
    /// enable (file transfers, proxies, cookie store, insecure TLS, a custom
    /// method outside `allowed_custom_methods`), a path outside `file_root`, a
    /// closed, evicted or foreign cookie session handle, or an idempotency key
    /// used while `max_idempotency_keys` is zero. Category `authorization`.
    ///
    /// [`EngineConfig`]: crate::EngineConfig
    PolicyViolation(ErrorDetail),
    /// `DNS_RESOLUTION_FAILED`: the host name did not resolve, or resolved to
    /// no address. Category `transient`, retry advice `safe` because no request
    /// left the engine.
    DnsResolution(ErrorDetail),
    /// `INVALID_HEADER`: a configured header name or value is not valid HTTP,
    /// or the request tries to set a header the engine must generate itself
    /// (multipart `Content-Type`, streaming `Content-Length`). Raised while
    /// preparing the request.
    InvalidHeader(ErrorDetail),
    /// `TIMEOUT`: the execution deadline (`options.deadline` or the
    /// [`ExecutionControl`](crate::ExecutionControl) deadline) or the HTTP
    /// `connect_timeout_ms` / `request_timeout_ms` of the engine expired. The remote effect is `unknown` and the retry advice is
    /// `quarantine`: the request may already have been processed.
    Timeout,
    /// `CANCELLED`: the [`CancellationToken`](crate::CancellationToken) of the
    /// execution was cancelled. The remote effect is `unknown` and the retry
    /// advice `quarantine`; active asynchronous jobs are cancelled remotely on
    /// a best-effort basis when the polling configuration asks for it.
    Cancelled,
    /// `ENGINE_CLOSED`: [`Engine::close`](crate::Engine::close) was called; the
    /// engine admits no new execution or cookie session operation.
    EngineClosed,
    /// `CIRCUIT_OPEN`: the circuit breaker for the destination is open, or a
    /// half-open probe is already in flight, so the request was not sent.
    /// Category `transient`, retry advice `safe`.
    CircuitOpen,
    /// `TRANSPORT_ERROR`: the HTTP exchange failed after it may have started
    /// (connection reset, TLS failure, body stream error). Remote effect
    /// `unknown`, retry advice `quarantine`.
    Transport(ErrorDetail),
    /// `RESPONSE_TOO_LARGE`: a buffered response body exceeded
    /// `EngineConfig::max_response_bytes`, either by its declared
    /// `Content-Length` or while it was being read. Reading stops at the limit.
    ResponseTooLarge {
        /// The byte limit that was exceeded; reported as `details.limit_bytes`.
        limit_bytes: usize,
    },
    /// `REQUEST_TOO_LARGE`: an encoded request body exceeded
    /// `EngineConfig::max_request_bytes`; the request was not sent.
    RequestTooLarge {
        /// The byte limit that was exceeded; reported as `details.limit_bytes`.
        limit_bytes: usize,
    },
    /// `FILE_TOO_LARGE`: a downloaded or uploaded file exceeded the transfer
    /// limit (`input.file.max_bytes`, capped by
    /// `EngineConfig::max_file_transfer_bytes`). A download stops writing at
    /// the limit.
    FileTooLarge {
        /// The byte limit that was exceeded; reported as `details.limit_bytes`.
        limit_bytes: u64,
    },
    /// `FILE_IO`: a local file operation failed. Only the kind of the failure
    /// is kept (not found, permission denied, already exists, ...), never the
    /// operating system message or the path.
    FileIo(ErrorDetail),
    /// `CHECKSUM_MISMATCH`: the SHA-256 of a transferred file differs from
    /// `expected_sha256`, or an uploaded file changed while it was being sent.
    /// Raised in the finalize phase with remote effect `unknown`.
    ChecksumMismatch,
    /// `HTTP_STATUS`: the final response status is outside 2xx and not listed
    /// in `connection.success_statuses`, once the retry policy allows no further
    /// attempt. Remote effect `unknown`.
    HttpStatus {
        /// The HTTP status code; reported as `details.http_status`.
        status: u16,
    },
    /// `INVALID_RESPONSE`: the response body could not be decoded in the
    /// configured format, or lacks a value the configuration requires. A parse
    /// failure records only its line and column, never the body.
    InvalidResponse(ErrorDetail),
    /// `APPLICATION_ERROR`: the HTTP exchange succeeded but the payload
    /// reports a failure: a value at `connection.response.error_path` or an unmet
    /// `connection.response.success_when` condition. Category `execution`.
    Application(ErrorDetail),
    /// `MISSING_PARAMETER`: a required parameter has no value from the input
    /// record, its mapping or its default. Category `data_mapping`.
    MissingParameter(ErrorDetail),
    /// `AUTHENTICATION_FAILED`: obtaining a token failed (token endpoint
    /// returned an error, an unsuccessful status, invalid JSON or no token
    /// field, or a non-bearer OAuth token type).
    Authentication(ErrorDetail),
    /// `IDEMPOTENCY_CONFLICT`: the idempotency key was already admitted by this
    /// engine for a request with a different fingerprint. Raised before the
    /// network.
    IdempotencyConflict,
    /// `POLLING_TIMEOUT`: an asynchronous job did not reach a terminal state
    /// within `connection.polling.max_attempts` or `max_wait_ms`. Retry advice
    /// `requires_recovery`: the job may still be running remotely.
    PollingTimeout {
        /// Poll requests actually issued; reported as `details.poll_attempts`.
        attempts: u32,
    },
    /// `PAGINATION_LIMIT_REACHED`: pagination stopped at `max_rows` or
    /// `max_pages` while the source still had data; the rows read are returned
    /// with this error, so the result is `partial`. Category `resource_limit`.
    PaginationLimit {
        /// The row limit in force; reported as `details.max_rows`.
        max_rows: usize,
        /// The page limit in force for cursor and link pagination; reported as
        /// `details.max_pages`.
        max_pages: Option<usize>,
    },
    /// `DEADLINE_EXPIRED`: the execution deadline had already passed when the
    /// operation was admitted; nothing was resolved or sent. Category
    /// `timeout`, phase `validate`, remote effect `none`, retry `never`.
    DeadlineExpired,
    /// `DOWNLOAD_WRITE_FAILED`: writing a download to local storage failed
    /// after the HTTP request was sent. The remote side may have acted on it:
    /// remote effect `unknown`, retry `requires_recovery`.
    DownloadWrite(ErrorDetail),
    /// `CLEANUP_AFTER_PUBLISH_FAILED`: the download was published to its sink,
    /// then removing the local staging file failed. Phase `cleanup`, remote
    /// effect `committed`, retry `never`.
    CleanupAfterPublish(ErrorDetail),
    /// `RUNTIME_ERROR`: an internal invariant of the engine was violated (for
    /// example a value that could not be serialized). Reported instead of a
    /// panic; category `internal`.
    Runtime(ErrorDetail),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.public_message())
    }
}

impl std::error::Error for EngineError {}

/// Coarse classification of a failure, serialized in snake case as the
/// `category` of the `plenora-error-v1` error object.
///
/// The enum covers the categories the REST engine can report; the shared
/// error contract defines further values (`crs`, `not_found`, `conflict`, ...)
/// that this engine never produces. Each [`EngineError`] variant maps to
/// exactly one category.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// `invalid_plan`: the execution plan is inconsistent. Part of the shared
    /// vocabulary; no [`EngineError`] variant maps to it today.
    InvalidPlan,
    /// `invalid_configuration`: the request or its configuration is invalid
    /// (`INVALID_INPUT`, `INVALID_URL`, `INVALID_HEADER`,
    /// `IDEMPOTENCY_CONFLICT`).
    InvalidConfiguration,
    /// `schema`: the data does not match an expected schema. Part of the
    /// shared vocabulary; no [`EngineError`] variant maps to it today.
    Schema,
    /// `data_mapping`: an input record cannot supply a required value
    /// (`MISSING_PARAMETER`).
    DataMapping,
    /// `unsupported`: the contract version is not implemented
    /// (`UNSUPPORTED_SCHEMA`).
    Unsupported,
    /// `authentication`: credentials could not be obtained or were refused by
    /// the token endpoint (`AUTHENTICATION_FAILED`).
    Authentication,
    /// `authorization`: an engine policy refused the destination or the
    /// feature (`UNSAFE_ADDRESS`, `POLICY_VIOLATION`).
    Authorization,
    /// `timeout`: a deadline, an HTTP timeout or the polling budget expired
    /// (`TIMEOUT`, `POLLING_TIMEOUT`).
    Timeout,
    /// `cancelled`: the caller cancelled the execution (`CANCELLED`).
    Cancelled,
    /// `resource_limit`: a byte limit was exceeded (`RESPONSE_TOO_LARGE`,
    /// `REQUEST_TOO_LARGE`, `FILE_TOO_LARGE`).
    ResourceLimit,
    /// `io`: a local file operation failed (`FILE_IO`).
    Io,
    /// `protocol`: the remote response or transferred data is not what the
    /// protocol requires (`INVALID_RESPONSE`, `CHECKSUM_MISMATCH`).
    Protocol,
    /// `transient`: a failure that may clear on its own (`DNS_RESOLUTION_FAILED`,
    /// `TRANSPORT_ERROR`, `CIRCUIT_OPEN`).
    Transient,
    /// `execution`: the remote service or the engine state refused the work
    /// (`HTTP_STATUS`, `APPLICATION_ERROR`, `ENGINE_CLOSED`).
    Execution,
    /// `internal`: an engine invariant was violated (`RUNTIME_ERROR`).
    Internal,
}

/// Stage of the execution in which a failure happened, serialized in snake
/// case as the `phase` of the error object.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPhase {
    /// `validate`: before any network activity, while checking the request,
    /// the engine policies, the idempotency key and the deadline.
    Validate,
    /// `connect`: resolving the host, consulting the circuit breaker or
    /// obtaining an authentication token.
    Connect,
    /// `probe`: part of the shared vocabulary; no [`EngineError`] variant
    /// maps to it today.
    Probe,
    /// `prepare`: building the HTTP request (headers, parameters, body size).
    Prepare,
    /// `read`: sending the request and reading or interpreting the response,
    /// polling included.
    Read,
    /// `write`: a local file operation, such as writing a download.
    Write,
    /// `finalize`: verifying a completed transfer (SHA-256 checksum).
    Finalize,
    /// `cleanup`: cancellation handling, removing a download's staging file
    /// after publication, and internal failures that are not tied to a
    /// specific stage.
    Cleanup,
}

/// What the failure may have done on the remote side, serialized in snake case
/// as the `remote_effect` of the error object.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEffect {
    /// `none`: reported for the failure kinds the engine classifies as not
    /// changing remote state (validation, policy, request preparation,
    /// connection and authentication, local I/O and internal failures), and
    /// only while no request of the operation, or of the input record, was
    /// sent; afterwards the same failures report `unknown` (ERR-014).
    None,
    /// `partial`: part of the work was applied remotely. Part of the shared
    /// vocabulary; the engine does not report it today.
    Partial,
    /// `committed`: the work was applied. The engine reports it for a
    /// download that was published to its sink before a later step (removing
    /// the staging file) failed.
    Committed,
    /// `unknown`: a request may have reached the remote service (timeout,
    /// cancellation, transport failure, error status or payload, size or
    /// checksum failure after sending); the caller cannot assume either way.
    Unknown,
}

/// Retry strategy suggested for a failure, serialized in snake case as
/// `retry.kind`.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryKind {
    /// `never`: repeating the same request fails the same way.
    Never,
    /// `quarantine`: the remote effect is unknown; retry only after checking
    /// the remote state or with an idempotency key the service honors
    /// (`TIMEOUT`, `CANCELLED`, `TRANSPORT_ERROR`).
    Quarantine,
    /// `safe`: nothing was sent, so the request can be repeated as is
    /// (`DNS_RESOLUTION_FAILED`, `CIRCUIT_OPEN`).
    Safe,
    /// `requires_recovery`: an asynchronous job may still be running; resume
    /// it from the recovery data in `ExecutionResult::recoveries` instead of
    /// submitting it again (`POLLING_TIMEOUT`).
    RequiresRecovery,
}

/// Retry advice of the error object, serialized as `{"kind": ...}`.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct RetryAdvice {
    /// The suggested retry strategy.
    pub kind: RetryKind,
}

impl RetryAdvice {
    pub(crate) const NEVER: Self = Self {
        kind: RetryKind::Never,
    };
    const QUARANTINE: Self = Self {
        kind: RetryKind::Quarantine,
    };
    const SAFE: Self = Self {
        kind: RetryKind::Safe,
    };
    const REQUIRES_RECOVERY: Self = Self {
        kind: RetryKind::RequiresRecovery,
    };
}

/// Events retained independently for the operation and each input record.
#[derive(Clone, Copy)]
pub(crate) enum ExecutionEvent {
    Entered(ErrorPhase),
    RequestSent,
}

/// Reduction of execution events, independent of their count and scheduling.
/// A cached response can enter a phase without ever sending a request.
#[derive(Clone, Copy, Default)]
pub(crate) struct FailureContext {
    pub(crate) entered: Option<ErrorPhase>,
    sent: bool,
}

impl FailureContext {
    pub(crate) fn observe(&mut self, event: ExecutionEvent) {
        match event {
            ExecutionEvent::Entered(phase) => self.entered = Some(phase),
            ExecutionEvent::RequestSent => self.sent = true,
        }
    }

    /// ERR-003 and ERR-014 are applied together, once the execution has ended.
    /// Existing unknown/committed/partial effects and their retry advice stay
    /// intact. A record's events never determine the operation's phase.
    pub(crate) fn apply(self, error: &mut ExecutionError) {
        if error.phase == ErrorPhase::Validate
            && let Some(entered) = self.entered
        {
            error.phase = entered;
        }
        if self.sent && error.remote_effect == RemoteEffect::None {
            error.remote_effect = RemoteEffect::Unknown;
            error.retry = RetryAdvice::REQUIRES_RECOVERY;
            if error.phase == ErrorPhase::Validate {
                error.phase = ErrorPhase::Read;
            }
        }
    }
}

/// Preserve the already resolved tuple when crossing the runtime boundary.
pub(crate) fn execution_error_payload(error: &ExecutionError) -> ErrorPayload {
    ErrorPayload {
        category: error.category,
        phase: error.phase,
        remote_effect: error.remote_effect,
        retry: error.retry,
        code: error.code.clone(),
        message: error.message.clone(),
        details: error.details.clone(),
    }
}

/// Serializable form of an [`EngineError`]: the `plenora-error-v1` object
/// carried by a runtime error envelope and, with an optional `input_index`,
/// by each entry of `ExecutionResult::errors`.
///
/// It contains only engine-chosen values; the `ErrorDetail` text of the
/// variant is not part of it.
#[derive(Clone, Debug, Serialize)]
pub struct ErrorPayload {
    /// Coarse classification of the failure.
    pub category: ErrorCategory,
    /// Stage of the execution in which the failure happened.
    pub phase: ErrorPhase,
    /// What the failure may have done on the remote side.
    pub remote_effect: RemoteEffect,
    /// Suggested retry strategy.
    pub retry: RetryAdvice,
    /// Stable machine-readable code in upper snake case, such as
    /// `HTTP_STATUS` or `INVALID_INPUT`.
    pub code: String,
    /// Static English message of the variant, identical to its `Display`
    /// text; never contains remote or local data.
    pub message: String,
    /// Engine-chosen numbers of the variant: `received_version` and
    /// `supported_version`, `limit_bytes`, `http_status` or `poll_attempts`.
    /// Empty for the other variants. The runtime binding adds `async_jobs`
    /// with the recovery data of jobs still active when an execution failed.
    pub details: BTreeMap<String, Value>,
}

// Each variant has one complete static row. Dynamic numeric details and the
// duplicate-member diagnostic do not change its classification.
struct ErrorDescriptor {
    code: &'static str,
    message: &'static str,
    category: ErrorCategory,
    phase: ErrorPhase,
    remote_effect: RemoteEffect,
    retry: RetryAdvice,
}

impl EngineError {
    /// Builds the serializable error object for this failure: code, static
    /// message, category, phase, remote effect, retry advice and numeric
    /// details are all derived from the variant.
    pub fn payload(&self) -> ErrorPayload {
        let descriptor = self.descriptor();
        ErrorPayload {
            category: descriptor.category,
            phase: descriptor.phase,
            remote_effect: descriptor.remote_effect,
            retry: descriptor.retry,
            code: descriptor.code.to_owned(),
            message: self.public_message().to_owned(),
            details: self.details(),
        }
    }

    pub(crate) fn execution_error(&self, input_index: Option<usize>) -> ExecutionError {
        let payload = self.payload();
        ExecutionError {
            category: payload.category,
            phase: payload.phase,
            remote_effect: payload.remote_effect,
            retry: payload.retry,
            code: payload.code,
            message: payload.message,
            input_index,
            details: payload.details,
        }
    }

    fn descriptor(&self) -> ErrorDescriptor {
        use ErrorCategory as C;
        use ErrorPhase as P;
        use RemoteEffect as E;
        use RetryAdvice as R;
        let (code, message, category, phase, remote_effect, retry) = match self {
            Self::InvalidInput(_) => (
                "INVALID_INPUT",
                "REST input is invalid",
                C::InvalidConfiguration,
                P::Validate,
                E::None,
                R::NEVER,
            ),
            Self::UnsupportedSchema { .. } => (
                "UNSUPPORTED_SCHEMA",
                "REST contract version is unsupported",
                C::Unsupported,
                P::Validate,
                E::None,
                R::NEVER,
            ),
            Self::InvalidUrl(_) => (
                "INVALID_URL",
                "REST URL is invalid",
                C::InvalidConfiguration,
                P::Validate,
                E::None,
                R::NEVER,
            ),
            Self::UnsafeAddress(_) => (
                "UNSAFE_ADDRESS",
                "Outbound address is not allowed",
                C::Authorization,
                P::Validate,
                E::None,
                R::NEVER,
            ),
            Self::PolicyViolation(_) => (
                "POLICY_VIOLATION",
                "Security policy denied the request",
                C::Authorization,
                P::Validate,
                E::None,
                R::NEVER,
            ),
            Self::DnsResolution(_) => (
                "DNS_RESOLUTION_FAILED",
                "DNS resolution failed",
                C::Transient,
                P::Connect,
                E::None,
                R::SAFE,
            ),
            Self::InvalidHeader(_) => (
                "INVALID_HEADER",
                "HTTP header is invalid",
                C::InvalidConfiguration,
                P::Prepare,
                E::None,
                R::NEVER,
            ),
            Self::Timeout => (
                "TIMEOUT",
                "REST execution timed out",
                C::Timeout,
                P::Read,
                E::Unknown,
                R::QUARANTINE,
            ),
            Self::Cancelled => (
                "CANCELLED",
                "REST execution was cancelled",
                C::Cancelled,
                P::Cleanup,
                E::Unknown,
                R::QUARANTINE,
            ),
            Self::EngineClosed => (
                "ENGINE_CLOSED",
                "REST engine is closed",
                C::Execution,
                P::Validate,
                E::None,
                R::NEVER,
            ),
            Self::CircuitOpen { .. } => (
                "CIRCUIT_OPEN",
                "Circuit breaker is open",
                C::Transient,
                P::Connect,
                E::None,
                R::SAFE,
            ),
            Self::Transport(_) => (
                "TRANSPORT_ERROR",
                "HTTP transport failed",
                C::Transient,
                P::Read,
                E::Unknown,
                R::QUARANTINE,
            ),
            Self::ResponseTooLarge { .. } => (
                "RESPONSE_TOO_LARGE",
                "Response exceeded its byte limit",
                C::ResourceLimit,
                P::Read,
                E::Unknown,
                R::NEVER,
            ),
            Self::RequestTooLarge { .. } => (
                "REQUEST_TOO_LARGE",
                "Request exceeded its byte limit",
                C::ResourceLimit,
                P::Prepare,
                E::None,
                R::NEVER,
            ),
            Self::FileTooLarge { .. } => (
                "FILE_TOO_LARGE",
                "File transfer exceeded its byte limit",
                C::ResourceLimit,
                P::Read,
                E::Unknown,
                R::NEVER,
            ),
            Self::FileIo(_) => (
                "FILE_IO",
                "File operation failed",
                C::Io,
                P::Write,
                E::None,
                R::NEVER,
            ),
            Self::ChecksumMismatch { .. } => (
                "CHECKSUM_MISMATCH",
                "SHA-256 checksum verification failed",
                C::Protocol,
                P::Finalize,
                E::Unknown,
                R::NEVER,
            ),
            Self::HttpStatus { .. } => (
                "HTTP_STATUS",
                "Remote service returned an unsuccessful status",
                C::Execution,
                P::Read,
                E::Unknown,
                R::NEVER,
            ),
            Self::InvalidResponse(_) => (
                "INVALID_RESPONSE",
                "Remote response is invalid",
                C::Protocol,
                P::Read,
                E::Unknown,
                R::NEVER,
            ),
            Self::Application(_) => (
                "APPLICATION_ERROR",
                "Remote application reported failure",
                C::Execution,
                P::Read,
                E::Unknown,
                R::NEVER,
            ),
            Self::MissingParameter(_) => (
                "MISSING_PARAMETER",
                "A required parameter is missing",
                C::DataMapping,
                P::Prepare,
                E::None,
                R::NEVER,
            ),
            Self::Authentication(_) => (
                "AUTHENTICATION_FAILED",
                "Authentication failed",
                C::Authentication,
                P::Connect,
                E::None,
                R::NEVER,
            ),
            Self::IdempotencyConflict => (
                "IDEMPOTENCY_CONFLICT",
                "Idempotency key conflicts with prior input",
                C::InvalidConfiguration,
                P::Validate,
                E::None,
                R::NEVER,
            ),
            Self::PollingTimeout { .. } => (
                "POLLING_TIMEOUT",
                "Asynchronous operation did not complete",
                C::Timeout,
                P::Read,
                E::Unknown,
                R::REQUIRES_RECOVERY,
            ),
            Self::PaginationLimit { .. } => (
                "PAGINATION_LIMIT_REACHED",
                "Pagination stopped at a configured limit with data remaining",
                C::ResourceLimit,
                P::Read,
                E::None,
                R::NEVER,
            ),
            Self::DeadlineExpired => (
                "DEADLINE_EXPIRED",
                "Execution deadline had already passed",
                C::Timeout,
                P::Validate,
                E::None,
                R::NEVER,
            ),
            Self::DownloadWrite(_) => (
                "DOWNLOAD_WRITE_FAILED",
                "Download could not be written after the request was sent",
                C::Io,
                P::Write,
                E::Unknown,
                R::REQUIRES_RECOVERY,
            ),
            Self::CleanupAfterPublish(_) => (
                "CLEANUP_AFTER_PUBLISH_FAILED",
                "Download was published but its staging file could not be removed",
                C::Io,
                P::Cleanup,
                E::Committed,
                R::NEVER,
            ),
            Self::Runtime(_) => (
                "RUNTIME_ERROR",
                "REST engine failed internally",
                C::Internal,
                P::Cleanup,
                E::None,
                R::NEVER,
            ),
        };
        ErrorDescriptor {
            code,
            message,
            category,
            phase,
            remote_effect,
            retry,
        }
    }

    fn public_message(&self) -> &'static str {
        match self {
            Self::InvalidInput(detail) | Self::Authentication(detail)
                if detail.text() == "control document contains duplicate JSON members" =>
            {
                "control document contains duplicate JSON members"
            }
            _ => self.descriptor().message,
        }
    }

    fn details(&self) -> BTreeMap<String, Value> {
        match self {
            Self::UnsupportedSchema {
                received,
                supported,
            } => BTreeMap::from([
                ("received_version".to_owned(), json!(received)),
                ("supported_version".to_owned(), json!(supported)),
            ]),
            Self::ResponseTooLarge { limit_bytes } | Self::RequestTooLarge { limit_bytes } => {
                BTreeMap::from([("limit_bytes".to_owned(), json!(limit_bytes))])
            }
            Self::FileTooLarge { limit_bytes } => {
                BTreeMap::from([("limit_bytes".to_owned(), json!(limit_bytes))])
            }
            Self::HttpStatus { status } => {
                BTreeMap::from([("http_status".to_owned(), json!(status))])
            }
            Self::PollingTimeout { attempts } => {
                BTreeMap::from([("poll_attempts".to_owned(), json!(attempts))])
            }
            Self::PaginationLimit {
                max_rows,
                max_pages,
            } => {
                let mut details = BTreeMap::from([("max_rows".to_owned(), json!(max_rows))]);
                if let Some(max_pages) = max_pages {
                    details.insert("max_pages".to_owned(), json!(max_pages));
                }
                details
            }
            _ => BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EngineError, ErrorDetail};

    #[test]
    fn event_reduction_preserves_the_whole_tuple() {
        use super::{
            ErrorPhase as P, ExecutionEvent, FailureContext, RemoteEffect as E, RetryKind as R,
        };
        // Independent transition cases: cache-only, sent without a recorded
        // phase, later local refusal, known unknown and committed effects.
        let cases = [
            (
                EngineError::InvalidUrl("x".into()),
                None,
                false,
                P::Validate,
                E::None,
                R::Never,
            ),
            (
                EngineError::InvalidUrl("x".into()),
                Some(P::Connect),
                false,
                P::Connect,
                E::None,
                R::Never,
            ),
            (
                EngineError::InvalidUrl("x".into()),
                None,
                true,
                P::Read,
                E::Unknown,
                R::RequiresRecovery,
            ),
            (
                EngineError::InvalidUrl("x".into()),
                Some(P::Prepare),
                true,
                P::Prepare,
                E::Unknown,
                R::RequiresRecovery,
            ),
            (
                EngineError::CircuitOpen,
                Some(P::Read),
                false,
                P::Connect,
                E::None,
                R::Safe,
            ),
            (
                EngineError::CircuitOpen,
                Some(P::Read),
                true,
                P::Connect,
                E::Unknown,
                R::RequiresRecovery,
            ),
            (
                EngineError::Cancelled,
                Some(P::Read),
                true,
                P::Cleanup,
                E::Unknown,
                R::Quarantine,
            ),
            (
                EngineError::CleanupAfterPublish("x".into()),
                Some(P::Read),
                true,
                P::Cleanup,
                E::Committed,
                R::Never,
            ),
        ];
        for (variant, entered, sent, phase, effect, retry) in cases {
            let mut context = FailureContext::default();
            if sent {
                context.observe(ExecutionEvent::RequestSent);
            }
            if let Some(entered) = entered {
                context.observe(ExecutionEvent::Entered(entered));
            }
            let mut error = variant.execution_error(Some(7));
            context.apply(&mut error);
            assert_eq!(
                (error.phase, error.remote_effect, error.retry.kind),
                (phase, effect, retry)
            );
            let once = serde_json::to_value(&error).unwrap();
            context.apply(&mut error);
            assert_eq!(serde_json::to_value(&error).unwrap(), once);
            let payload = super::execution_error_payload(&error);
            assert_eq!(
                (payload.phase, payload.remote_effect, payload.retry.kind),
                (phase, effect, retry)
            );
            assert_eq!(payload.code, variant.payload().code);
            assert_eq!(error.input_index, Some(7));
        }
    }

    #[test]
    fn local_failures_after_a_request_keep_the_remote_effect_honest() {
        use super::{ErrorCategory, ErrorPhase, RemoteEffect, RetryKind};
        // Shared runtime matrix, case 9a/9d: the request went out, the local
        // write failed; nothing proves the remote side did not act.
        let write = EngineError::DownloadWrite(ErrorDetail::from("disk full")).payload();
        assert_eq!(write.category, ErrorCategory::Io);
        assert_eq!(write.phase, ErrorPhase::Write);
        assert_eq!(write.remote_effect, RemoteEffect::Unknown);
        assert_eq!(write.retry.kind, RetryKind::RequiresRecovery);
        // Case 9e: published, then local cleanup failed.
        let cleanup =
            EngineError::CleanupAfterPublish(ErrorDetail::from("staging file kept")).payload();
        assert_eq!(cleanup.category, ErrorCategory::Io);
        assert_eq!(cleanup.phase, ErrorPhase::Cleanup);
        assert_eq!(cleanup.remote_effect, RemoteEffect::Committed);
        assert_eq!(cleanup.retry.kind, RetryKind::Never);
        // Case 7b: an expired deadline before invocation.
        let expired = EngineError::DeadlineExpired.payload();
        assert_eq!(expired.category, ErrorCategory::Timeout);
        assert_eq!(expired.phase, ErrorPhase::Validate);
        assert_eq!(expired.remote_effect, RemoteEffect::None);
        assert_eq!(expired.retry.kind, RetryKind::Never);
    }

    #[test]
    fn display_is_the_static_public_message() {
        let errors = [
            EngineError::Transport(ErrorDetail::from("connection reset")),
            EngineError::Application(ErrorDetail::from("error_path reported a failure")),
            EngineError::InvalidResponse(ErrorDetail::at("response body is not valid JSON", 3, 7)),
            EngineError::CircuitOpen,
            EngineError::ChecksumMismatch,
        ];
        for error in errors {
            assert_eq!(error.to_string(), error.payload().message);
        }
    }

    #[test]
    fn a_parse_detail_carries_only_its_position() {
        let detail = ErrorDetail::at("response body is not valid JSON", 3, 7);
        assert_eq!(detail.text(), "response body is not valid JSON");
        assert_eq!(detail.position(), Some((3, 7)));
        assert_eq!(ErrorDetail::from("x").position(), None);
    }
}
