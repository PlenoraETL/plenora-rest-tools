//! Self-contained REST execution engine.
//!
//! The public boundary deliberately exposes only versioned configuration,
//! execution requests, results, metrics, and engine-owned errors. HTTP client
//! types never cross this boundary.

#![forbid(unsafe_code)]

mod contract;
mod engine;
mod error;
mod json_path;
mod response_body;
mod transport;

pub use contract::{
    ApiKeyLocation, AuthConfig, BatchConfig, BatchInputFormat, BodyType, CachePolicy,
    CircuitBreakerPolicy, ConnectionConfig, CookiePolicy, EngineConfig, ExecutionError,
    ExecutionInput, ExecutionMetrics, ExecutionOperation, ExecutionOptions, ExecutionOutput,
    ExecutionRequest, ExecutionResult, ExecutionStatus, HttpMethod, HttpResponseMetadata,
    IterationSpec, JsonObject, OAuthClientAuth, OutputMapping, PaginationConfig, ParameterLocation,
    ParameterMode, ParameterSpec, PollingConfig, ProxyConfig, QuerySerialization, QueryStyle,
    RequestConfig, ResponseConfig, ResponseFormat, ResponseTransform, RetryPolicy, SCHEMA_VERSION,
    TlsConfig,
};
pub use engine::Engine;
pub use error::{EngineError, ErrorPayload};
