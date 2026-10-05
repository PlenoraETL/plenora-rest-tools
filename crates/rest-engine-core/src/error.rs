use std::{collections::BTreeMap, fmt};

use serde::Serialize;
use serde_json::{Value, json};

use crate::ExecutionError;

/// Diagnostic text carried by an [`EngineError`] variant.
///
/// The text routinely contains what a remote party or the environment said:
/// a response body excerpt, a remote error message, an address, a domain, an
/// `io::Error`, a checksum of the caller's data. None of it may leave the
/// engine, so the type is opaque: it can be built from a string (`.into()`),
/// it is never displayed, its `Debug` is redacted, and no accessor returns
/// it. Errors are described to callers only by [`EngineError::payload`] and by
/// the static `Display`.
pub struct ErrorDetail(
    // Kept for in-crate diagnostics in tests; never formatted or returned.
    #[cfg_attr(not(test), allow(dead_code))] String,
);

impl ErrorDetail {
    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.0
    }
}

impl From<String> for ErrorDetail {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ErrorDetail {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Debug for ErrorDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Every failure the engine reports.
///
/// Variants that describe a failure in words carry an opaque [`ErrorDetail`];
/// variants that carry numbers chosen by the engine or the protocol (a byte
/// limit, an HTTP status, a poll count, a contract version) keep them public.
/// `Display` is the static public message of the variant, the same text as
/// `payload().message`, so formatting an error never prints remote or local
/// data.
#[derive(Debug)]
pub enum EngineError {
    InvalidInput(ErrorDetail),
    UnsupportedSchema {
        received: u32,
        supported: u32,
    },
    InvalidUrl(ErrorDetail),
    UnsafeAddress(ErrorDetail),
    PolicyViolation(ErrorDetail),
    DnsResolution(ErrorDetail),
    InvalidHeader(ErrorDetail),
    Timeout,
    Cancelled,
    EngineClosed,
    CircuitOpen {
        origin: ErrorDetail,
    },
    Transport(ErrorDetail),
    ResponseTooLarge {
        limit_bytes: usize,
    },
    RequestTooLarge {
        limit_bytes: usize,
    },
    FileTooLarge {
        limit_bytes: u64,
    },
    FileIo(ErrorDetail),
    ChecksumMismatch {
        expected: ErrorDetail,
        actual: ErrorDetail,
    },
    HttpStatus {
        status: u16,
    },
    InvalidResponse(ErrorDetail),
    Application(ErrorDetail),
    MissingParameter(ErrorDetail),
    Authentication(ErrorDetail),
    IdempotencyConflict,
    PollingTimeout {
        attempts: u32,
    },
    Runtime(ErrorDetail),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.public_message())
    }
}

impl std::error::Error for EngineError {}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    InvalidPlan,
    InvalidConfiguration,
    Schema,
    DataMapping,
    Unsupported,
    Authentication,
    Authorization,
    Timeout,
    Cancelled,
    ResourceLimit,
    Io,
    Protocol,
    Transient,
    Execution,
    Internal,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPhase {
    Validate,
    Connect,
    Probe,
    Prepare,
    Read,
    Write,
    Finalize,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEffect {
    None,
    Partial,
    Committed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryKind {
    Never,
    Quarantine,
    Safe,
    RequiresRecovery,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct RetryAdvice {
    pub kind: RetryKind,
}

impl RetryAdvice {
    const NEVER: Self = Self {
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

#[derive(Clone, Debug, Serialize)]
pub struct ErrorPayload {
    pub category: ErrorCategory,
    pub phase: ErrorPhase,
    pub remote_effect: RemoteEffect,
    pub retry: RetryAdvice,
    pub code: String,
    pub message: String,
    pub details: BTreeMap<String, Value>,
}

impl EngineError {
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
            Self::Timeout | Self::PollingTimeout { .. } => ErrorCategory::Timeout,
            Self::Cancelled => ErrorCategory::Cancelled,
            Self::EngineClosed => ErrorCategory::Execution,
            Self::ResponseTooLarge { .. }
            | Self::RequestTooLarge { .. }
            | Self::FileTooLarge { .. } => ErrorCategory::ResourceLimit,
            Self::FileIo(_) => ErrorCategory::Io,
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
            | Self::IdempotencyConflict => ErrorPhase::Validate,
            Self::DnsResolution(_) | Self::CircuitOpen { .. } | Self::Authentication(_) => {
                ErrorPhase::Connect
            }
            Self::InvalidHeader(_) | Self::RequestTooLarge { .. } | Self::MissingParameter(_) => {
                ErrorPhase::Prepare
            }
            Self::FileIo(_) => ErrorPhase::Write,
            Self::ChecksumMismatch { .. } => ErrorPhase::Finalize,
            Self::Cancelled => ErrorPhase::Cleanup,
            Self::Timeout
            | Self::Transport(_)
            | Self::ResponseTooLarge { .. }
            | Self::FileTooLarge { .. }
            | Self::HttpStatus { .. }
            | Self::InvalidResponse(_)
            | Self::Application(_)
            | Self::PollingTimeout { .. } => ErrorPhase::Read,
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
            | Self::PollingTimeout { .. } => RemoteEffect::Unknown,
            _ => RemoteEffect::None,
        }
    }

    fn retry(&self) -> RetryAdvice {
        match self {
            Self::Timeout | Self::Cancelled | Self::Transport(_) => RetryAdvice::QUARANTINE,
            Self::PollingTimeout { .. } => RetryAdvice::REQUIRES_RECOVERY,
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
            _ => BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EngineError, ErrorDetail};

    #[test]
    fn formatting_an_error_never_prints_its_detail() {
        let secret = "remote said: token=abc123 from 10.0.0.7";
        let errors = [
            EngineError::Transport(ErrorDetail::from(secret)),
            EngineError::Application(ErrorDetail::from(secret)),
            EngineError::FileIo(ErrorDetail::from(secret)),
            EngineError::CircuitOpen {
                origin: ErrorDetail::from(secret),
            },
            EngineError::ChecksumMismatch {
                expected: ErrorDetail::from(secret),
                actual: ErrorDetail::from(secret),
            },
        ];
        for error in errors {
            let display = error.to_string();
            let debug = format!("{error:?}");
            let alternate = format!("{error:#?}");
            for rendered in [&display, &debug, &alternate] {
                assert!(!rendered.contains("abc123"), "{rendered}");
                assert!(!rendered.contains("10.0.0.7"), "{rendered}");
            }
            assert_eq!(display, error.payload().message);
        }
    }

    #[test]
    fn the_detail_stays_available_inside_the_crate() {
        let detail = ErrorDetail::from(String::from("diagnostic"));
        assert_eq!(detail.text(), "diagnostic");
    }
}
