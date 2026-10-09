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

/// Corrects the axes of an error raised after a request of the operation (or
/// of its input record) was sent (Typed Errors 1.0, ERR-014).
///
/// The axes of a variant describe where it is normally raised: before the
/// network, for most of those that report `remote_effect: none`. Raised once a
/// request has gone out, whatever its method and its answer (a redirect, an
/// OAuth token request included), the same failure cannot prove that the
/// remote side did nothing: a cross-origin redirect after a POST, a polling
/// URL refused after the job was accepted, a source file that cannot be
/// reopened for a second attempt. Such an error reports `unknown` and
/// `requires_recovery`. Category, code, message and details are kept: they
/// still say what failed.
///
/// The phase is the variant's own (ERR-003, the last phase known to have
/// started): `write` for a local file that cannot be reopened, `connect` for
/// a token request that failed, `read` for a pagination limit. The one
/// exception is `validate`, which means "before any network activity" and
/// cannot hold once a request was sent: those variants (a redirect or a
/// polling URL refused) are raised while the engine interprets a response,
/// so the phase that had started is `read`, which covers sending a request
/// and reading its answer. An error that already reports an effect
/// (`unknown`, `committed`, `partial`) keeps all its axes.
pub(crate) fn after_sent_request(error: &mut ExecutionError) {
    if error.remote_effect == RemoteEffect::None {
        error.remote_effect = RemoteEffect::Unknown;
        error.retry = RetryAdvice::REQUIRES_RECOVERY;
        if error.phase == ErrorPhase::Validate {
            error.phase = ErrorPhase::Read;
        }
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

impl EngineError {
    /// Builds the serializable error object for this failure: code, static
    /// message, category, phase, remote effect, retry advice and numeric
    /// details are all derived from the variant.
    pub fn payload(&self) -> ErrorPayload {
        ErrorPayload {
            category: self.category(),
            phase: self.phase(),
            remote_effect: self.remote_effect(),
            retry: self.retry(),
            code: self.code().to_owned(),
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

    fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "INVALID_INPUT",
            Self::UnsupportedSchema { .. } => "UNSUPPORTED_SCHEMA",
            Self::InvalidUrl(_) => "INVALID_URL",
            Self::UnsafeAddress(_) => "UNSAFE_ADDRESS",
            Self::PolicyViolation(_) => "POLICY_VIOLATION",
            Self::DnsResolution(_) => "DNS_RESOLUTION_FAILED",
            Self::InvalidHeader(_) => "INVALID_HEADER",
            Self::Timeout => "TIMEOUT",
            Self::Cancelled => "CANCELLED",
            Self::EngineClosed => "ENGINE_CLOSED",
            Self::CircuitOpen { .. } => "CIRCUIT_OPEN",
            Self::Transport(_) => "TRANSPORT_ERROR",
            Self::ResponseTooLarge { .. } => "RESPONSE_TOO_LARGE",
            Self::RequestTooLarge { .. } => "REQUEST_TOO_LARGE",
            Self::FileTooLarge { .. } => "FILE_TOO_LARGE",
            Self::FileIo(_) => "FILE_IO",
            Self::ChecksumMismatch { .. } => "CHECKSUM_MISMATCH",
            Self::HttpStatus { .. } => "HTTP_STATUS",
            Self::InvalidResponse(_) => "INVALID_RESPONSE",
            Self::Application(_) => "APPLICATION_ERROR",
            Self::MissingParameter(_) => "MISSING_PARAMETER",
            Self::Authentication(_) => "AUTHENTICATION_FAILED",
            Self::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            Self::PollingTimeout { .. } => "POLLING_TIMEOUT",
            Self::PaginationLimit { .. } => "PAGINATION_LIMIT_REACHED",

            Self::DeadlineExpired => "DEADLINE_EXPIRED",
            Self::DownloadWrite(_) => "DOWNLOAD_WRITE_FAILED",
            Self::CleanupAfterPublish(_) => "CLEANUP_AFTER_PUBLISH_FAILED",
            Self::Runtime(_) => "RUNTIME_ERROR",
        }
    }

    fn public_message(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "REST input is invalid",
            Self::UnsupportedSchema { .. } => "REST contract version is unsupported",
            Self::InvalidUrl(_) => "REST URL is invalid",
            Self::UnsafeAddress(_) => "Outbound address is not allowed",
            Self::PolicyViolation(_) => "Security policy denied the request",
            Self::DnsResolution(_) => "DNS resolution failed",
            Self::InvalidHeader(_) => "HTTP header is invalid",
            Self::Timeout => "REST execution timed out",
            Self::Cancelled => "REST execution was cancelled",
            Self::EngineClosed => "REST engine is closed",
            Self::CircuitOpen { .. } => "Circuit breaker is open",
            Self::Transport(_) => "HTTP transport failed",
            Self::ResponseTooLarge { .. } => "Response exceeded its byte limit",
            Self::RequestTooLarge { .. } => "Request exceeded its byte limit",
            Self::FileTooLarge { .. } => "File transfer exceeded its byte limit",
            Self::FileIo(_) => "File operation failed",
            Self::ChecksumMismatch { .. } => "SHA-256 checksum verification failed",
            Self::HttpStatus { .. } => "Remote service returned an unsuccessful status",
            Self::InvalidResponse(_) => "Remote response is invalid",
            Self::Application(_) => "Remote application reported failure",
            Self::MissingParameter(_) => "A required parameter is missing",
            Self::Authentication(_) => "Authentication failed",
            Self::IdempotencyConflict => "Idempotency key conflicts with prior input",
            Self::PollingTimeout { .. } => "Asynchronous operation did not complete",
            Self::PaginationLimit { .. } => {
                "Pagination stopped at a configured limit with data remaining"
            }
            Self::DeadlineExpired => "Execution deadline had already passed",
            Self::DownloadWrite(_) => "Download could not be written after the request was sent",
            Self::CleanupAfterPublish(_) => {
                "Download was published but its staging file could not be removed"
            }
            Self::Runtime(_) => "REST engine failed internally",
        }
    }

    fn category(&self) -> ErrorCategory {
        match self {
            Self::InvalidInput(_)
            | Self::InvalidUrl(_)
            | Self::InvalidHeader(_)
            | Self::IdempotencyConflict => ErrorCategory::InvalidConfiguration,
            Self::UnsupportedSchema { .. } => ErrorCategory::Unsupported,
            Self::UnsafeAddress(_) | Self::PolicyViolation(_) => ErrorCategory::Authorization,
            Self::DnsResolution(_) | Self::Transport(_) | Self::CircuitOpen { .. } => {
                ErrorCategory::Transient
            }
            Self::Timeout | Self::PollingTimeout { .. } | Self::DeadlineExpired => {
                ErrorCategory::Timeout
            }
            Self::Cancelled => ErrorCategory::Cancelled,
            Self::EngineClosed => ErrorCategory::Execution,
            Self::ResponseTooLarge { .. }
            | Self::RequestTooLarge { .. }
            | Self::FileTooLarge { .. }
            | Self::PaginationLimit { .. } => ErrorCategory::ResourceLimit,
            Self::FileIo(_) | Self::DownloadWrite(_) | Self::CleanupAfterPublish(_) => {
                ErrorCategory::Io
            }
            Self::ChecksumMismatch { .. } | Self::InvalidResponse(_) => ErrorCategory::Protocol,
            Self::HttpStatus { .. } | Self::Application(_) => ErrorCategory::Execution,
            Self::MissingParameter(_) => ErrorCategory::DataMapping,
            Self::Authentication(_) => ErrorCategory::Authentication,
            Self::Runtime(_) => ErrorCategory::Internal,
        }
    }

    fn phase(&self) -> ErrorPhase {
        match self {
            Self::InvalidInput(_)
            | Self::UnsupportedSchema { .. }
            | Self::InvalidUrl(_)
            | Self::UnsafeAddress(_)
            | Self::PolicyViolation(_)
            | Self::EngineClosed
            | Self::IdempotencyConflict
            | Self::DeadlineExpired => ErrorPhase::Validate,
            Self::DnsResolution(_) | Self::CircuitOpen { .. } | Self::Authentication(_) => {
                ErrorPhase::Connect
            }
            Self::InvalidHeader(_) | Self::RequestTooLarge { .. } | Self::MissingParameter(_) => {
                ErrorPhase::Prepare
            }
            Self::FileIo(_) | Self::DownloadWrite(_) => ErrorPhase::Write,
            Self::CleanupAfterPublish(_) => ErrorPhase::Cleanup,
            Self::ChecksumMismatch { .. } => ErrorPhase::Finalize,
            Self::Cancelled => ErrorPhase::Cleanup,
            Self::Timeout
            | Self::Transport(_)
            | Self::ResponseTooLarge { .. }
            | Self::FileTooLarge { .. }
            | Self::HttpStatus { .. }
            | Self::InvalidResponse(_)
            | Self::Application(_)
            | Self::PollingTimeout { .. }
            | Self::PaginationLimit { .. } => ErrorPhase::Read,
            Self::Runtime(_) => ErrorPhase::Cleanup,
        }
    }

    fn remote_effect(&self) -> RemoteEffect {
        match self {
            Self::Timeout
            | Self::Cancelled
            | Self::Transport(_)
            | Self::ResponseTooLarge { .. }
            | Self::FileTooLarge { .. }
            | Self::ChecksumMismatch { .. }
            | Self::HttpStatus { .. }
            | Self::InvalidResponse(_)
            | Self::Application(_)
            | Self::PollingTimeout { .. }
            | Self::DownloadWrite(_) => RemoteEffect::Unknown,
            // The publication happened; only local staging is left over.
            Self::CleanupAfterPublish(_) => RemoteEffect::Committed,
            _ => RemoteEffect::None,
        }
    }

    fn retry(&self) -> RetryAdvice {
        match self {
            Self::Timeout | Self::Cancelled | Self::Transport(_) => RetryAdvice::QUARANTINE,
            Self::PollingTimeout { .. } | Self::DownloadWrite(_) => RetryAdvice::REQUIRES_RECOVERY,
            Self::DnsResolution(_) | Self::CircuitOpen { .. } => RetryAdvice::SAFE,
            _ => RetryAdvice::NEVER,
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
