use serde::Serialize;
use thiserror::Error;

use crate::ExecutionError;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unsupported schema version {received}; supported version is {supported}")]
    UnsupportedSchema { received: u32, supported: u32 },
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("outbound address is not allowed: {0}")]
    UnsafeAddress(String),
    #[error("security policy denied the request: {0}")]
    PolicyViolation(String),
    #[error("DNS resolution failed: {0}")]
    DnsResolution(String),
    #[error("invalid HTTP header: {0}")]
    InvalidHeader(String),
    #[error("request timed out")]
    Timeout,
    #[error("HTTP transport failed: {0}")]
    Transport(String),
    #[error("response exceeds the {limit_bytes} byte limit")]
    ResponseTooLarge { limit_bytes: usize },
    #[error("request body exceeds the {limit_bytes} byte limit")]
    RequestTooLarge { limit_bytes: usize },
    #[error("HTTP request failed with status {status}")]
    HttpStatus {
        status: u16,
        body_preview: Option<String>,
    },
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("application-level response failure: {0}")]
    Application(String),
    #[error("required parameter is missing: {0}")]
    MissingParameter(String),
    #[error("authentication failed: {0}")]
    Authentication(String),
    #[error("asynchronous operation did not complete after {attempts} polls")]
    PollingTimeout { attempts: u32 },
    #[error("engine runtime failed: {0}")]
    Runtime(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    pub retriable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_preview: Option<String>,
}

impl EngineError {
    pub fn payload(&self) -> ErrorPayload {
        ErrorPayload {
            code: self.code().to_owned(),
            message: self.to_string(),
            retriable: self.retriable(),
            http_status: self.http_status(),
            body_preview: self.body_preview().map(ToOwned::to_owned),
        }
    }

    pub(crate) fn execution_error(&self, input_index: Option<usize>) -> ExecutionError {
        let payload = self.payload();
        ExecutionError {
            code: payload.code,
            message: payload.message,
            retriable: payload.retriable,
            input_index,
            http_status: payload.http_status,
            body_preview: payload.body_preview,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "invalid_input",
            Self::UnsupportedSchema { .. } => "unsupported_schema",
            Self::InvalidUrl(_) => "invalid_url",
            Self::UnsafeAddress(_) => "unsafe_address",
            Self::PolicyViolation(_) => "policy_violation",
            Self::DnsResolution(_) => "dns_resolution_failed",
            Self::InvalidHeader(_) => "invalid_header",
            Self::Timeout => "timeout",
            Self::Transport(_) => "transport_error",
            Self::ResponseTooLarge { .. } => "response_too_large",
            Self::RequestTooLarge { .. } => "request_too_large",
            Self::HttpStatus { .. } => "http_status",
            Self::InvalidResponse(_) => "invalid_response",
            Self::Application(_) => "application_error",
            Self::MissingParameter(_) => "missing_parameter",
            Self::Authentication(_) => "authentication_failed",
            Self::PollingTimeout { .. } => "polling_timeout",
            Self::Runtime(_) => "runtime_error",
        }
    }

    fn retriable(&self) -> bool {
        match self {
            Self::Timeout
            | Self::Transport(_)
            | Self::DnsResolution(_)
            | Self::PollingTimeout { .. } => true,
            Self::HttpStatus { status, .. } => {
                *status == 408 || *status == 429 || (500..=599).contains(status)
            }
            _ => false,
        }
    }

    fn http_status(&self) -> Option<u16> {
        match self {
            Self::HttpStatus { status, .. } => Some(*status),
            _ => None,
        }
    }

    fn body_preview(&self) -> Option<&str> {
        match self {
            Self::HttpStatus { body_preview, .. } => body_preview.as_deref(),
            _ => None,
        }
    }
}
