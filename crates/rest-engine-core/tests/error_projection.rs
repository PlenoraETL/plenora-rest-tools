// Literal expectations transcribed from the pre-refactor 0.4.0 projections.
// This oracle never reads the descriptor table or derives expected axes from it.
use plenora_rest_core::EngineError;

#[test]
fn every_variant_preserves_its_public_tuple() {
    let cases = [
        (
            EngineError::InvalidInput("private diagnostic".into()),
            (
                "INVALID_INPUT",
                "REST input is invalid",
                "invalid_configuration",
                "validate",
                "none",
                "never",
            ),
        ),
        (
            EngineError::UnsupportedSchema {
                received: 2,
                supported: 1,
            },
            (
                "UNSUPPORTED_SCHEMA",
                "REST contract version is unsupported",
                "unsupported",
                "validate",
                "none",
                "never",
            ),
        ),
        (
            EngineError::InvalidUrl("private diagnostic".into()),
            (
                "INVALID_URL",
                "REST URL is invalid",
                "invalid_configuration",
                "validate",
                "none",
                "never",
            ),
        ),
        (
            EngineError::UnsafeAddress("private diagnostic".into()),
            (
                "UNSAFE_ADDRESS",
                "Outbound address is not allowed",
                "authorization",
                "validate",
                "none",
                "never",
            ),
        ),
        (
            EngineError::PolicyViolation("private diagnostic".into()),
            (
                "POLICY_VIOLATION",
                "Security policy denied the request",
                "authorization",
                "validate",
                "none",
                "never",
            ),
        ),
        (
            EngineError::DnsResolution("private diagnostic".into()),
            (
                "DNS_RESOLUTION_FAILED",
                "DNS resolution failed",
                "transient",
                "connect",
                "none",
                "safe",
            ),
        ),
        (
            EngineError::InvalidHeader("private diagnostic".into()),
            (
                "INVALID_HEADER",
                "HTTP header is invalid",
                "invalid_configuration",
                "prepare",
                "none",
                "never",
            ),
        ),
        (
            EngineError::Timeout,
            (
                "TIMEOUT",
                "REST execution timed out",
                "timeout",
                "read",
                "unknown",
                "quarantine",
            ),
        ),
        (
            EngineError::Cancelled,
            (
                "CANCELLED",
                "REST execution was cancelled",
                "cancelled",
                "cleanup",
                "unknown",
                "quarantine",
            ),
        ),
        (
            EngineError::EngineClosed,
            (
                "ENGINE_CLOSED",
                "REST engine is closed",
                "execution",
                "validate",
                "none",
                "never",
            ),
        ),
        (
            EngineError::CircuitOpen,
            (
                "CIRCUIT_OPEN",
                "Circuit breaker is open",
                "transient",
                "connect",
                "none",
                "safe",
            ),
        ),
        (
            EngineError::Transport("private diagnostic".into()),
            (
                "TRANSPORT_ERROR",
                "HTTP transport failed",
                "transient",
                "read",
                "unknown",
                "quarantine",
            ),
        ),
        (
            EngineError::ResponseTooLarge { limit_bytes: 7 },
            (
                "RESPONSE_TOO_LARGE",
                "Response exceeded its byte limit",
                "resource_limit",
                "read",
                "unknown",
                "never",
            ),
        ),
        (
            EngineError::RequestTooLarge { limit_bytes: 7 },
            (
                "REQUEST_TOO_LARGE",
                "Request exceeded its byte limit",
                "resource_limit",
                "prepare",
                "none",
                "never",
            ),
        ),
        (
            EngineError::FileTooLarge { limit_bytes: 7 },
            (
                "FILE_TOO_LARGE",
                "File transfer exceeded its byte limit",
                "resource_limit",
                "read",
                "unknown",
                "never",
            ),
        ),
        (
            EngineError::FileIo("private diagnostic".into()),
            (
                "FILE_IO",
                "File operation failed",
                "io",
                "write",
                "none",
                "never",
            ),
        ),
        (
            EngineError::ChecksumMismatch,
            (
                "CHECKSUM_MISMATCH",
                "SHA-256 checksum verification failed",
                "protocol",
                "finalize",
                "unknown",
                "never",
            ),
        ),
        (
            EngineError::HttpStatus { status: 503 },
            (
                "HTTP_STATUS",
                "Remote service returned an unsuccessful status",
                "execution",
                "read",
                "unknown",
                "never",
            ),
        ),
        (
            EngineError::InvalidResponse("private diagnostic".into()),
            (
                "INVALID_RESPONSE",
                "Remote response is invalid",
                "protocol",
                "read",
                "unknown",
                "never",
            ),
        ),
        (
            EngineError::Application("private diagnostic".into()),
            (
                "APPLICATION_ERROR",
                "Remote application reported failure",
                "execution",
                "read",
                "unknown",
                "never",
            ),
        ),
        (
            EngineError::MissingParameter("private diagnostic".into()),
            (
                "MISSING_PARAMETER",
                "A required parameter is missing",
                "data_mapping",
                "prepare",
                "none",
                "never",
            ),
        ),
        (
            EngineError::Authentication("private diagnostic".into()),
            (
                "AUTHENTICATION_FAILED",
                "Authentication failed",
                "authentication",
                "connect",
                "none",
                "never",
            ),
        ),
        (
            EngineError::IdempotencyConflict,
            (
                "IDEMPOTENCY_CONFLICT",
                "Idempotency key conflicts with prior input",
                "invalid_configuration",
                "validate",
                "none",
                "never",
            ),
        ),
        (
            EngineError::PollingTimeout { attempts: 3 },
            (
                "POLLING_TIMEOUT",
                "Asynchronous operation did not complete",
                "timeout",
                "read",
                "unknown",
                "requires_recovery",
            ),
        ),
        (
            EngineError::PaginationLimit {
                max_rows: 7,
                max_pages: Some(2),
            },
            (
                "PAGINATION_LIMIT_REACHED",
                "Pagination stopped at a configured limit with data remaining",
                "resource_limit",
                "read",
                "none",
                "never",
            ),
        ),
        (
            EngineError::DeadlineExpired,
            (
                "DEADLINE_EXPIRED",
                "Execution deadline had already passed",
                "timeout",
                "validate",
                "none",
                "never",
            ),
        ),
        (
            EngineError::DownloadWrite("private diagnostic".into()),
            (
                "DOWNLOAD_WRITE_FAILED",
                "Download could not be written after the request was sent",
                "io",
                "write",
                "unknown",
                "requires_recovery",
            ),
        ),
        (
            EngineError::CleanupAfterPublish("private diagnostic".into()),
            (
                "CLEANUP_AFTER_PUBLISH_FAILED",
                "Download was published but its staging file could not be removed",
                "io",
                "cleanup",
                "committed",
                "never",
            ),
        ),
        (
            EngineError::Runtime("private diagnostic".into()),
            (
                "RUNTIME_ERROR",
                "REST engine failed internally",
                "internal",
                "cleanup",
                "none",
                "never",
            ),
        ),
    ];
    for (error, (code, message, category, phase, effect, retry)) in cases {
        let payload = serde_json::to_value(error.payload()).unwrap();
        assert_eq!(payload["code"], code);
        assert_eq!(payload["message"], message);
        assert_eq!(payload["category"], category);
        assert_eq!(payload["phase"], phase);
        assert_eq!(payload["remote_effect"], effect);
        assert_eq!(payload["retry"]["kind"], retry);
        assert_eq!(error.to_string(), message);
    }
}
