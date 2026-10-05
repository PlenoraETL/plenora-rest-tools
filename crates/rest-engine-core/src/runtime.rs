use std::{collections::BTreeMap, fmt, path::PathBuf};

use crate::error::ErrorDetail;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::{
    AuthConfig, CAPABILITY_NAME, CancellationToken, Engine, EngineError, ErrorPayload,
    ExecutionControl, ExecutionOperation, ExecutionRequest, ExecutionResult, ExecutionStatus,
    FILE_TRANSFER_INPUT_CONTRACT, FILE_TRANSFER_RESULT_CONTRACT, IdempotencyLocation,
    ParameterLocation, REST_DOWNLOAD, REST_ENRICH, REST_GENERATE, REST_TEST, REST_UPLOAD,
    RUNTIME_BINDING_VERSION,
    capability::{
        EXECUTION_REQUEST_CONTRACT, EXECUTION_RESULT_CONTRACT, RUNTIME_INTERFACE_CONTRACT,
    },
    engine::{is_disallowed_idempotency_header, is_sensitive_header_name},
};

/// Envelope contract accepted on input as an alternative to
/// `plenora-runtime-binding-v1`, used by conformance vectors. Responses always
/// carry `plenora-runtime-binding-v1`.
pub const RUNTIME_VECTOR_CONTRACT: &str = "plenora-runtime-vector-v1";
/// Contract of the payload of an error envelope, written in its
/// `plenora.output.contract` metadata.
pub const ERROR_CONTRACT: &str = "plenora-error-v1";
/// Content type of request and success envelopes; a request with any other
/// content type is rejected.
pub const JSON_CONTENT_TYPE: &str = "application/json";
/// Content type of error envelopes.
pub const ERROR_CONTENT_TYPE: &str = "application/vnd.plenora.error+json";

/// Envelope exchanged with a Plenora runtime host (`plenora-runtime-binding-v1`).
///
/// Unknown fields are rejected when deserializing. `Debug` prints the metadata
/// keys only and never the payload, which can contain request data.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeMessage {
    /// Envelope version; must equal
    /// [`RUNTIME_BINDING_VERSION`](crate::RUNTIME_BINDING_VERSION) (1),
    /// otherwise `UNSUPPORTED_SCHEMA`.
    pub schema_version: u32,
    /// Envelope contract: `plenora-runtime-binding-v1` or
    /// [`RUNTIME_VECTOR_CONTRACT`] on input, always
    /// `plenora-runtime-binding-v1` on output.
    pub contract: String,
    /// Direction of the message; an invocation must be a `request`.
    pub kind: RuntimeMessageKind,
    /// [`JSON_CONTENT_TYPE`] for requests and successes,
    /// [`ERROR_CONTENT_TYPE`] for errors.
    pub content_type: String,
    /// String metadata. A request must carry `plenora.message.id` and
    /// `plenora.trace.correlation_id` (lowercase hyphenated UUIDs),
    /// `plenora.capability.name` (`plenora.rest-tools`),
    /// `plenora.capability.version` (`1`), `plenora.capability.operation`
    /// (`rest.test`, `rest.generate`, `rest.enrich`, `rest.download`,
    /// `rest.upload`), `plenora.operation.version` (`1`) and
    /// `plenora.input.contract` matching the operation; it may carry
    /// `plenora.message.causation_id`, `plenora.execution.deadline`
    /// (RFC 3339) and `plenora.execution.idempotency_key`. A response carries a
    /// new `plenora.message.id`, the request id as
    /// `plenora.message.causation_id`, the correlation id, the operation and
    /// its version, and `plenora.output.contract`.
    pub metadata: BTreeMap<String, String>,
    /// Request: an execution request object whose `operation` matches the
    /// selected operation; its idempotency key must come from the metadata,
    /// not from `options.idempotency_key`. Success: the execution result.
    /// Error: an [`ErrorPayload`] object.
    pub payload: Value,
}

impl fmt::Debug for RuntimeMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeMessage")
            .field("schema_version", &self.schema_version)
            .field("contract", &self.contract)
            .field("kind", &self.kind)
            .field("content_type", &self.content_type)
            .field("metadata_keys", &self.metadata.keys().collect::<Vec<_>>())
            .field("payload", &"<redacted>")
            .finish()
    }
}

/// Direction of a [`RuntimeMessage`], serialized in snake case.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMessageKind {
    /// An invocation sent by the host.
    Request,
    /// A completed execution; the payload is the execution result.
    Success,
    /// A refused or failed invocation; the payload is an error object.
    Error,
}

/// Host-provided resolution of the opaque references a runtime request may
/// contain.
///
/// Runtime requests cannot carry secrets or local paths inline: inline
/// authentication, sensitive headers or parameters, client private keys,
/// proxy credentials and local file paths are rejected with `INVALID_INPUT`.
/// They name references instead, which the binding passes to these methods
/// after checking that each is non-empty, at most 512 bytes, not absolute,
/// not a `file:` URL or drive path, and free of `..` segments. An error
/// returned by a method becomes the error envelope of the invocation.
pub trait RuntimeResources: Send + Sync {
    /// Resolves `connection.credential_ref` to the authentication to use.
    fn resolve_credentials(&self, reference: &str) -> Result<AuthConfig, EngineError>;
    /// Resolves the `artifact_source` of a `rest.upload` to the local file to
    /// read.
    fn resolve_artifact_source(&self, reference: &str) -> Result<PathBuf, EngineError>;
    /// Resolves the `artifact_sink` of a `rest.download` to the local file to
    /// write.
    fn resolve_artifact_sink(&self, reference: &str) -> Result<PathBuf, EngineError>;
}

/// Adapter that serves `plenora-runtime-binding-v1` envelopes with an
/// [`Engine`] and the [`RuntimeResources`] of the host.
pub struct RuntimeBinding<'engine, 'resources, Resources> {
    engine: &'engine Engine,
    resources: &'resources Resources,
}

impl<'engine, 'resources, Resources> RuntimeBinding<'engine, 'resources, Resources>
where
    Resources: RuntimeResources,
{
    /// Binds an engine to the host resources; both are borrowed.
    pub fn new(engine: &'engine Engine, resources: &'resources Resources) -> Self {
        Self { engine, resources }
    }

    /// Validates the envelope, resolves its references, runs the execution
    /// and returns the response envelope. Never fails as a Rust call: every
    /// refusal or failed execution is an `error` envelope whose payload is the
    /// error object of the first execution error (with `details.async_jobs`
    /// when asynchronous jobs remained active); otherwise a `success`
    /// envelope carries the execution result.
    pub async fn invoke(
        &self,
        request: RuntimeMessage,
        cancellation: CancellationToken,
    ) -> RuntimeMessage {
        match self.prepare_request(&request) {
            Ok((mut execution, operation, output_contract)) => {
                let control = match ExecutionControl::new(cancellation).with_optional_deadline(
                    request
                        .metadata
                        .get("plenora.execution.deadline")
                        .map(String::as_str),
                ) {
                    Ok(control) => control,
                    Err(error) => return error_message(&request, error.payload()),
                };
                if let Err(error) =
                    resolve_runtime_inputs(&mut execution, operation, self.resources)
                {
                    return error_message(&request, error.payload());
                }
                let result = self.engine.execute_with_control(execution, control).await;
                if result.status == ExecutionStatus::Failed {
                    let mut error = result
                        .errors
                        .first()
                        .map(execution_error_payload)
                        .unwrap_or_else(|| {
                            EngineError::Runtime(ErrorDetail::from(
                                "execution failed without an error",
                            ))
                            .payload()
                        });
                    if !result.recoveries.is_empty() {
                        error.details.insert(
                            "async_jobs".to_owned(),
                            serde_json::to_value(&result.recoveries)
                                .unwrap_or_else(|_| Value::Array(Vec::new())),
                        );
                    }
                    error_message(&request, error)
                } else {
                    success_message(&request, output_contract, result)
                }
            }
            Err(error) => error_message(&request, error.payload()),
        }
    }

    /// JSON form of [`invoke`](Self::invoke). Returns
    /// [`EngineError::InvalidInput`] when the text is not a valid envelope
    /// (only line and column are kept) and [`EngineError::Runtime`] when the
    /// response cannot be serialized; every other outcome is a serialized
    /// envelope.
    pub async fn invoke_json(
        &self,
        request_json: &str,
        cancellation: CancellationToken,
    ) -> Result<String, EngineError> {
        let request = serde_json::from_str::<RuntimeMessage>(request_json).map_err(|error| {
            EngineError::InvalidInput(ErrorDetail::at(
                "request is not valid JSON for the contract",
                error.line(),
                error.column(),
            ))
        })?;
        let response = self.invoke(request, cancellation).await;
        serde_json::to_string(&response).map_err(|_| {
            EngineError::Runtime(ErrorDetail::from("response could not be serialized"))
        })
    }

    fn prepare_request(
        &self,
        message: &RuntimeMessage,
    ) -> Result<(ExecutionRequest, ExecutionOperation, &'static str), EngineError> {
        validate_envelope(message)?;
        let operation_id = required_metadata(message, "plenora.capability.operation")?;
        let (operation, input_contract, output_contract) = operation_contracts(operation_id)?;
        if required_metadata(message, "plenora.input.contract")? != input_contract {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "runtime input contract does not match the operation",
            )));
        }
        // A JSON value has no line or column, and serde's message can quote
        // the payload, so only the fact is kept.
        let mut execution = serde_json::from_value::<ExecutionRequest>(message.payload.clone())
            .map_err(|_| {
                EngineError::InvalidInput(ErrorDetail::from(
                    "runtime payload does not match the execution request contract",
                ))
            })?;
        if execution.options.idempotency_key.is_some() {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "runtime idempotency key must use plenora.execution.idempotency_key metadata",
            )));
        }
        if let Some(key) = message.metadata.get("plenora.execution.idempotency_key") {
            execution.options.idempotency_key = Some(key.clone());
        }
        if execution.operation != operation {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "runtime selector and payload operation differ",
            )));
        }
        Ok((execution, operation, output_contract))
    }
}

fn validate_envelope(message: &RuntimeMessage) -> Result<(), EngineError> {
    if message.schema_version != RUNTIME_BINDING_VERSION {
        return Err(EngineError::UnsupportedSchema {
            received: message.schema_version,
            supported: RUNTIME_BINDING_VERSION,
        });
    }
    if message.contract != RUNTIME_INTERFACE_CONTRACT && message.contract != RUNTIME_VECTOR_CONTRACT
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime envelope contract is unsupported",
        )));
    }
    if message.kind != RuntimeMessageKind::Request {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime invocation requires a request envelope",
        )));
    }
    if message.content_type != JSON_CONTENT_TYPE || !message.payload.is_object() {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime request payload must be a JSON object",
        )));
    }
    validate_uuid_metadata(message, "plenora.message.id")?;
    validate_uuid_metadata(message, "plenora.trace.correlation_id")?;
    if message
        .metadata
        .contains_key("plenora.message.causation_id")
    {
        validate_uuid_metadata(message, "plenora.message.causation_id")?;
    }
    if required_metadata(message, "plenora.capability.name")? != CAPABILITY_NAME {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime capability name is not plenora.rest-tools",
        )));
    }
    if required_metadata(message, "plenora.capability.version")?
        != RUNTIME_BINDING_VERSION.to_string()
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime capability version is unsupported",
        )));
    }
    if required_metadata(message, "plenora.operation.version")? != "1" {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime operation version is unsupported",
        )));
    }
    Ok(())
}

fn required_metadata<'a>(message: &'a RuntimeMessage, key: &str) -> Result<&'a str, EngineError> {
    message
        .metadata
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from("runtime metadata lacks a required key"))
        })
}

fn validate_uuid_metadata(message: &RuntimeMessage, key: &str) -> Result<(), EngineError> {
    let value = required_metadata(message, key)?;
    let parsed = Uuid::parse_str(value).map_err(|_| {
        EngineError::InvalidInput(ErrorDetail::from("runtime metadata UUID is not canonical"))
    })?;
    if parsed.hyphenated().to_string() != value {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime metadata UUID must be lowercase and hyphenated",
        )));
    }
    Ok(())
}

fn operation_contracts(
    id: &str,
) -> Result<(ExecutionOperation, &'static str, &'static str), EngineError> {
    match id {
        REST_TEST => Ok((
            ExecutionOperation::Test,
            EXECUTION_REQUEST_CONTRACT,
            EXECUTION_RESULT_CONTRACT,
        )),
        REST_GENERATE => Ok((
            ExecutionOperation::Generate,
            EXECUTION_REQUEST_CONTRACT,
            EXECUTION_RESULT_CONTRACT,
        )),
        REST_ENRICH => Ok((
            ExecutionOperation::Enrich,
            EXECUTION_REQUEST_CONTRACT,
            EXECUTION_RESULT_CONTRACT,
        )),
        REST_DOWNLOAD => Ok((
            ExecutionOperation::Download,
            FILE_TRANSFER_INPUT_CONTRACT,
            FILE_TRANSFER_RESULT_CONTRACT,
        )),
        REST_UPLOAD => Ok((
            ExecutionOperation::Upload,
            FILE_TRANSFER_INPUT_CONTRACT,
            FILE_TRANSFER_RESULT_CONTRACT,
        )),
        _ => Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime operation is unknown",
        ))),
    }
}

fn resolve_runtime_inputs<Resources: RuntimeResources>(
    request: &mut ExecutionRequest,
    operation: ExecutionOperation,
    resources: &Resources,
) -> Result<(), EngineError> {
    reject_inline_secrets(request)?;
    if let Some(reference) = request.connection.credential_ref.take() {
        validate_reference(&reference)?;
        request.connection.auth = resources.resolve_credentials(&reference)?;
    }

    match operation {
        ExecutionOperation::Download => {
            let file = request.input.file.as_mut().ok_or_else(|| {
                EngineError::InvalidInput(ErrorDetail::from("REST download requires file input"))
            })?;
            if !file.path.is_empty() {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "runtime download cannot contain a local path",
                )));
            }
            if file.artifact_source.is_some() {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "REST download forbids artifact_source",
                )));
            }
            let reference = file
                .artifact_sink
                .as_ref()
                .ok_or_else(|| {
                    EngineError::InvalidInput(ErrorDetail::from(
                        "REST download requires artifact_sink",
                    ))
                })?
                .reference
                .clone();
            validate_reference(&reference)?;
            file.path = resources
                .resolve_artifact_sink(&reference)?
                .to_string_lossy()
                .into_owned();
        }
        ExecutionOperation::Upload => {
            let file = request.input.file.as_mut().ok_or_else(|| {
                EngineError::InvalidInput(ErrorDetail::from("REST upload requires file input"))
            })?;
            if !file.path.is_empty() {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "runtime upload cannot contain a local path",
                )));
            }
            if file.artifact_sink.is_some() {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "REST upload forbids artifact_sink",
                )));
            }
            let reference = file
                .artifact_source
                .as_ref()
                .ok_or_else(|| {
                    EngineError::InvalidInput(ErrorDetail::from(
                        "REST upload requires artifact_source",
                    ))
                })?
                .reference
                .clone();
            validate_reference(&reference)?;
            file.path = resources
                .resolve_artifact_source(&reference)?
                .to_string_lossy()
                .into_owned();
        }
        _ if request.input.file.is_some() => {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "execution operation cannot carry a file transfer",
            )));
        }
        _ => {}
    }
    Ok(())
}

fn reject_inline_secrets(request: &ExecutionRequest) -> Result<(), EngineError> {
    if !matches!(&request.connection.auth, AuthConfig::None) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime request must use credential_ref instead of inline authentication",
        )));
    }
    if request
        .connection
        .headers
        .keys()
        .any(|name| is_sensitive_header_name(name))
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime request contains a sensitive HTTP header",
        )));
    }
    // Blocking only `connection.headers` leaves the same channel open through
    // the parameter list: a fixed or mapped parameter with `location: "header"`
    // is turned into an HTTP header later on, and a `location: "cookie"`
    // parameter is turned into a Cookie header. Both must go through
    // `credential_ref` like every other secret.
    if request
        .connection
        .parameters
        .iter()
        .any(|parameter| match parameter.location {
            ParameterLocation::Header => is_sensitive_header_name(&parameter.name),
            ParameterLocation::Cookie => true,
            _ => false,
        })
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime request contains a sensitive parameter; use credential_ref",
        )));
    }
    // The idempotency header name is caller configured and the engine inserts a
    // header with it, so it is the same channel again: naming it `Authorization`
    // would inject an auth header the runtime boundary otherwise refuses.
    // `is_disallowed_idempotency_header` trims before classifying, matching what
    // `apply_idempotency` does when it inserts the header.
    if request.connection.idempotency.location == IdempotencyLocation::Header
        && is_disallowed_idempotency_header(&request.connection.idempotency.name)
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime idempotency header name must not read as a credential",
        )));
    }
    if request.connection.tls.client_identity_pem.is_some() {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime request contains inline private key material",
        )));
    }
    if let Some(proxy) = &request.connection.proxy {
        let url = Url::parse(&proxy.url)
            .map_err(|_| EngineError::InvalidInput(ErrorDetail::from("proxy URL is invalid")))?;
        if proxy.username.is_some()
            || proxy.password.is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "runtime proxy credentials must use a secret reference",
            )));
        }
    }
    Ok(())
}

fn validate_reference(reference: &str) -> Result<(), EngineError> {
    let normalized = reference.replace('\\', "/");
    if reference.is_empty()
        || reference.len() > 512
        || normalized.starts_with('/')
        || normalized.to_ascii_lowercase().starts_with("file:")
        || normalized
            .as_bytes()
            .get(1)
            .is_some_and(|value| *value == b':')
        || normalized.split('/').any(|segment| segment == "..")
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime reference must be opaque and authorized",
        )));
    }
    Ok(())
}

fn success_message(
    request: &RuntimeMessage,
    output_contract: &str,
    result: ExecutionResult,
) -> RuntimeMessage {
    RuntimeMessage {
        schema_version: RUNTIME_BINDING_VERSION,
        contract: RUNTIME_INTERFACE_CONTRACT.to_owned(),
        kind: RuntimeMessageKind::Success,
        content_type: JSON_CONTENT_TYPE.to_owned(),
        metadata: response_metadata(request, output_contract),
        payload: serde_json::to_value(result).unwrap_or_else(|_| Value::Object(Default::default())),
    }
}

fn error_message(request: &RuntimeMessage, error: ErrorPayload) -> RuntimeMessage {
    RuntimeMessage {
        schema_version: RUNTIME_BINDING_VERSION,
        contract: RUNTIME_INTERFACE_CONTRACT.to_owned(),
        kind: RuntimeMessageKind::Error,
        content_type: ERROR_CONTENT_TYPE.to_owned(),
        metadata: response_metadata(request, ERROR_CONTRACT),
        payload: serde_json::to_value(error).unwrap_or_else(|_| Value::Object(Default::default())),
    }
}

fn response_metadata(request: &RuntimeMessage, output_contract: &str) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::from([
        ("plenora.message.id".to_owned(), Uuid::new_v4().to_string()),
        (
            "plenora.trace.correlation_id".to_owned(),
            request
                .metadata
                .get("plenora.trace.correlation_id")
                .cloned()
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
        ),
        (
            "plenora.operation.version".to_owned(),
            request
                .metadata
                .get("plenora.operation.version")
                .cloned()
                .unwrap_or_else(|| "1".to_owned()),
        ),
        (
            "plenora.output.contract".to_owned(),
            output_contract.to_owned(),
        ),
    ]);
    if let Some(operation) = request.metadata.get("plenora.capability.operation") {
        metadata.insert("plenora.capability.operation".to_owned(), operation.clone());
    }
    if let Some(message_id) = request.metadata.get("plenora.message.id") {
        metadata.insert(
            "plenora.message.causation_id".to_owned(),
            message_id.clone(),
        );
    }
    metadata
}

fn execution_error_payload(error: &crate::ExecutionError) -> ErrorPayload {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{RuntimeResources, validate_reference};
    use crate::error::ErrorDetail;
    use crate::{AuthConfig, EngineError};

    struct EmptyResources;

    impl RuntimeResources for EmptyResources {
        fn resolve_credentials(&self, _reference: &str) -> Result<AuthConfig, EngineError> {
            Ok(AuthConfig::None)
        }

        fn resolve_artifact_source(&self, _reference: &str) -> Result<PathBuf, EngineError> {
            Err(EngineError::InvalidInput(ErrorDetail::from(
                "missing source",
            )))
        }

        fn resolve_artifact_sink(&self, _reference: &str) -> Result<PathBuf, EngineError> {
            Err(EngineError::InvalidInput(ErrorDetail::from("missing sink")))
        }
    }

    #[test]
    fn runtime_references_are_opaque_and_not_paths() {
        assert!(validate_reference("artifact://tenant/item").is_ok());
        assert!(validate_reference("../private/file").is_err());
        assert!(validate_reference("C:\\private\\file").is_err());
        let _resources = EmptyResources;
    }
}
