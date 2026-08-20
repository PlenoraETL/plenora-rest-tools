//! Self-contained REST execution engine.
//!
//! The public boundary deliberately exposes only versioned configuration,
//! execution requests, results, metrics, and engine-owned errors. HTTP client
//! types never cross this boundary.

#![forbid(unsafe_code)]

mod capability;
mod contract;
mod control;
mod engine;
mod error;
mod json_path;
mod response_body;
mod runtime;
mod transport;

pub use capability::{
    CAPABILITY_ATTRIBUTES_CONTRACT, CAPABILITY_NAME, CAPABILITY_SCHEMA_VERSION, COMPONENT_ID,
    CapabilityDocument, CapabilityInterface, CapabilityStatus, EXECUTION_REQUEST_CONTRACT,
    EXECUTION_RESULT_CONTRACT, ExecutionControls, FILE_TRANSFER_INPUT_CONTRACT,
    FILE_TRANSFER_RESULT_CONTRACT, OperationCapability, PYTHON_INTERFACE_CONTRACT,
    PayloadCapability, REST_DOWNLOAD, REST_ENRICH, REST_GENERATE, REST_TEST, REST_UPLOAD,
    RUNTIME_BINDING_VERSION, RUNTIME_INTERFACE_CONTRACT, RUST_INTERFACE_CONTRACT, SideEffect,
    Surface, capabilities,
};
pub use contract::{
    ASYNC_JOB_RECOVERY_CONTRACT, ApiKeyLocation, ArtifactReference, AsyncJobRecovery, AuthConfig,
    BatchConfig, BatchInputFormat, BodyType, CachePolicy, CircuitBreakerPolicy, ConnectionConfig,
    CookiePolicy, EngineConfig, ExecutionError, ExecutionInput, ExecutionMetrics,
    ExecutionOperation, ExecutionOptions, ExecutionOutput, ExecutionRequest, ExecutionResult,
    ExecutionStatus, FileTransferDirection, FileTransferInput, HttpMethod, HttpResponseMetadata,
    IdempotencyConfig, IdempotencyLocation, IntegrityMetadata, IterationSpec, JsonObject,
    OAuthClientAuth, OutputMapping, PaginationConfig, ParameterLocation, ParameterMode,
    ParameterSpec, PollingCancelConfig, PollingConfig, PollingResumeConfig, ProxyConfig,
    QuerySerialization, QueryStyle, RequestConfig, ResponseConfig, ResponseFormat,
    ResponseTransform, RetryPolicy, SCHEMA_VERSION, TlsConfig,
};
pub use control::{CancellationToken, ExecutionControl};
pub use engine::Engine;
pub use error::{
    EngineError, ErrorCategory, ErrorPayload, ErrorPhase, RemoteEffect, RetryAdvice, RetryKind,
};
pub use runtime::{
    ERROR_CONTENT_TYPE, ERROR_CONTRACT, JSON_CONTENT_TYPE, RUNTIME_VECTOR_CONTRACT, RuntimeBinding,
    RuntimeMessage, RuntimeMessageKind, RuntimeResources,
};
