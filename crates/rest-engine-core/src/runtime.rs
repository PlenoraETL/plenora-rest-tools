use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::{
    AuthConfig, CAPABILITY_NAME, CancellationToken, Engine, EngineError, ErrorPayload,
    ExecutionControl, ExecutionOperation, ExecutionRequest, ExecutionResult, ExecutionStatus,
    FILE_TRANSFER_INPUT_CONTRACT, FILE_TRANSFER_RESULT_CONTRACT, REST_DOWNLOAD, REST_ENRICH,
    REST_GENERATE, REST_TEST, REST_UPLOAD, RUNTIME_BINDING_VERSION,
    capability::{
        EXECUTION_REQUEST_CONTRACT, EXECUTION_RESULT_CONTRACT, RUNTIME_INTERFACE_CONTRACT,
    },
};

pub const RUNTIME_VECTOR_CONTRACT: &str = "plenora-runtime-vector-v1";
pub const ERROR_CONTRACT: &str = "plenora-error-v1";
pub const JSON_CONTENT_TYPE: &str = "application/json";
pub const ERROR_CONTENT_TYPE: &str = "application/vnd.plenora.error+json";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeMessage {
    pub schema_version: u32,
    pub contract: String,
    pub kind: RuntimeMessageKind,
    pub content_type: String,
    pub metadata: BTreeMap<String, String>,
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMessageKind {
    Request,
    Success,
    Error,
}

pub trait RuntimeResources: Send + Sync {
    fn resolve_credentials(&self, reference: &str) -> Result<AuthConfig, EngineError>;
    fn resolve_artifact_source(&self, reference: &str) -> Result<PathBuf, EngineError>;
    fn resolve_artifact_sink(&self, reference: &str) -> Result<PathBuf, EngineError>;
}

pub struct RuntimeBinding<'engine, 'resources, Resources> {
    engine: &'engine Engine,
    resources: &'resources Resources,
}

impl<'engine, 'resources, Resources> RuntimeBinding<'engine, 'resources, Resources>
where
    Resources: RuntimeResources,
{
    pub fn new(engine: &'engine Engine, resources: &'resources Resources) -> Self {
        Self { engine, resources }
    }

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
                    let error = result
                        .errors
                        .first()
                        .map(execution_error_payload)
                        .unwrap_or_else(|| {
                            EngineError::Runtime("execution failed without an error".to_owned())
                                .payload()
                        });
                    error_message(&request, error)
                } else {
                    success_message(&request, output_contract, result)
                }
            }
            Err(error) => error_message(&request, error.payload()),
        }
    }

    pub async fn invoke_json(
        &self,
        request_json: &str,
        cancellation: CancellationToken,
    ) -> Result<String, EngineError> {
        let request = serde_json::from_str::<RuntimeMessage>(request_json)
            .map_err(|error| EngineError::InvalidInput(error.to_string()))?;
        let response = self.invoke(request, cancellation).await;
        serde_json::to_string(&response).map_err(|error| EngineError::Runtime(error.to_string()))
    }

    fn prepare_request(
        &self,
        message: &RuntimeMessage,
    ) -> Result<(ExecutionRequest, ExecutionOperation, &'static str), EngineError> {
        validate_envelope(message)?;
        let operation_id = required_metadata(message, "plenora.capability.operation")?;
        let (operation, input_contract, output_contract) = operation_contracts(operation_id)?;
        if required_metadata(message, "plenora.input.contract")? != input_contract {
            return Err(EngineError::InvalidInput(
                "runtime input contract does not match the operation".to_owned(),
            ));
        }
        let execution = serde_json::from_value::<ExecutionRequest>(message.payload.clone())
            .map_err(|error| EngineError::InvalidInput(error.to_string()))?;
        if execution.operation != operation {
            return Err(EngineError::InvalidInput(
                "runtime selector and payload operation differ".to_owned(),
            ));
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
        return Err(EngineError::InvalidInput(
            "runtime envelope contract is unsupported".to_owned(),
        ));
    }
    if message.kind != RuntimeMessageKind::Request {
        return Err(EngineError::InvalidInput(
            "runtime invocation requires a request envelope".to_owned(),
        ));
    }
    if message.content_type != JSON_CONTENT_TYPE || !message.payload.is_object() {
        return Err(EngineError::InvalidInput(
            "runtime request payload must be a JSON object".to_owned(),
        ));
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
        return Err(EngineError::InvalidInput(
            "runtime capability name is not plenora.rest-tools".to_owned(),
        ));
    }
    if required_metadata(message, "plenora.capability.version")?
        != RUNTIME_BINDING_VERSION.to_string()
    {
        return Err(EngineError::InvalidInput(
            "runtime capability version is unsupported".to_owned(),
        ));
    }
    if required_metadata(message, "plenora.operation.version")? != "1" {
        return Err(EngineError::InvalidInput(
            "runtime operation version is unsupported".to_owned(),
        ));
    }
    Ok(())
}

fn required_metadata<'a>(message: &'a RuntimeMessage, key: &str) -> Result<&'a str, EngineError> {
    message
        .metadata
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| EngineError::InvalidInput(format!("runtime metadata lacks {key}")))
}

fn validate_uuid_metadata(message: &RuntimeMessage, key: &str) -> Result<(), EngineError> {
    let value = required_metadata(message, key)?;
    let parsed = Uuid::parse_str(value)
        .map_err(|_| EngineError::InvalidInput(format!("{key} must be a canonical UUID")))?;
    if parsed.hyphenated().to_string() != value {
        return Err(EngineError::InvalidInput(format!(
            "{key} must be a lowercase hyphenated UUID"
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
        _ => Err(EngineError::InvalidInput(
            "runtime operation is unknown".to_owned(),
        )),
    }
}

fn resolve_runtime_inputs<Resources: RuntimeResources>(
    request: &mut ExecutionRequest,
    operation: ExecutionOperation,
    resources: &Resources,
) -> Result<(), EngineError> {
    reject_inline_secrets(request)?;
    if let Some(reference) = request.connection.credential_ref.take() {
        validate_reference(&reference, "credential_ref")?;
        request.connection.auth = resources.resolve_credentials(&reference)?;
    }

    match operation {
        ExecutionOperation::Download => {
            let file = request.input.file.as_mut().ok_or_else(|| {
                EngineError::InvalidInput("REST download requires file input".to_owned())
            })?;
            if !file.path.is_empty() {
                return Err(EngineError::InvalidInput(
                    "runtime download cannot contain a local path".to_owned(),
                ));
            }
            if file.artifact_source.is_some() {
                return Err(EngineError::InvalidInput(
                    "REST download forbids artifact_source".to_owned(),
                ));
            }
            let reference = file
                .artifact_sink
                .as_ref()
                .ok_or_else(|| {
                    EngineError::InvalidInput("REST download requires artifact_sink".to_owned())
                })?
                .reference
                .clone();
            validate_reference(&reference, "artifact_sink")?;
            file.path = resources
                .resolve_artifact_sink(&reference)?
                .to_string_lossy()
                .into_owned();
        }
        ExecutionOperation::Upload => {
            let file = request.input.file.as_mut().ok_or_else(|| {
                EngineError::InvalidInput("REST upload requires file input".to_owned())
            })?;
            if !file.path.is_empty() {
                return Err(EngineError::InvalidInput(
                    "runtime upload cannot contain a local path".to_owned(),
                ));
            }
            if file.artifact_sink.is_some() {
                return Err(EngineError::InvalidInput(
                    "REST upload forbids artifact_sink".to_owned(),
                ));
            }
            let reference = file
                .artifact_source
                .as_ref()
                .ok_or_else(|| {
                    EngineError::InvalidInput("REST upload requires artifact_source".to_owned())
                })?
                .reference
                .clone();
            validate_reference(&reference, "artifact_source")?;
            file.path = resources
                .resolve_artifact_source(&reference)?
                .to_string_lossy()
                .into_owned();
        }
        _ if request.input.file.is_some() => {
            return Err(EngineError::InvalidInput(
                "execution operation cannot carry a file transfer".to_owned(),
            ));
        }
        _ => {}
    }
    Ok(())
}

fn reject_inline_secrets(request: &ExecutionRequest) -> Result<(), EngineError> {
    if !matches!(&request.connection.auth, AuthConfig::None) {
        return Err(EngineError::InvalidInput(
            "runtime request must use credential_ref instead of inline authentication".to_owned(),
        ));
    }
    if request.connection.headers.keys().any(|name| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "proxy-authorization" | "cookie" | "x-api-key"
        )
    }) {
        return Err(EngineError::InvalidInput(
            "runtime request contains a sensitive HTTP header".to_owned(),
        ));
    }
    if request.connection.tls.client_identity_pem.is_some() {
        return Err(EngineError::InvalidInput(
            "runtime request contains inline private key material".to_owned(),
        ));
    }
    if let Some(proxy) = &request.connection.proxy {
        let url = Url::parse(&proxy.url)
            .map_err(|_| EngineError::InvalidInput("proxy URL is invalid".to_owned()))?;
        if proxy.username.is_some()
            || proxy.password.is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(EngineError::InvalidInput(
                "runtime proxy credentials must use a secret reference".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_reference(reference: &str, field: &str) -> Result<(), EngineError> {
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
        return Err(EngineError::InvalidInput(format!(
            "{field} must be an opaque authorized reference"
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
    use crate::{AuthConfig, EngineError};

    struct EmptyResources;

    impl RuntimeResources for EmptyResources {
        fn resolve_credentials(&self, _reference: &str) -> Result<AuthConfig, EngineError> {
            Ok(AuthConfig::None)
        }

        fn resolve_artifact_source(&self, _reference: &str) -> Result<PathBuf, EngineError> {
            Err(EngineError::InvalidInput("missing source".to_owned()))
        }

        fn resolve_artifact_sink(&self, _reference: &str) -> Result<PathBuf, EngineError> {
            Err(EngineError::InvalidInput("missing sink".to_owned()))
        }
    }

    #[test]
    fn runtime_references_are_opaque_and_not_paths() {
        assert!(validate_reference("artifact://tenant/item", "artifact").is_ok());
        assert!(validate_reference("../private/file", "artifact").is_err());
        assert!(validate_reference("C:\\private\\file", "artifact").is_err());
        let _resources = EmptyResources;
    }
}
