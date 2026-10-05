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

/// Longest reference the runtime accepts, in bytes.
const MAX_REFERENCE_BYTES: usize = 512;

/// Accepts only an opaque `scheme:` or `scheme://` reference.
///
/// The rule is positive, as in the adopted contracts (Runtime Binding 1.0
/// RT-013 and the `reference` grammar of `data-execution-input-v3`): a
/// lowercase scheme of 2 to 32 characters, a colon, an optional `//`, then a
/// non-empty remainder without whitespace or backslashes; never `file:`, never
/// a `.` or `..` segment, never a percent-encoded dot. Anything else is refused
/// whatever it looks like, so a relative path such as `dir/report.csv` or
/// `report.csv` cannot pass for a handle the way it could pass a list of
/// forbidden spellings. The host still decides, in `RuntimeResources`, which of
/// the well-formed references it authorizes.
fn validate_reference(reference: &str) -> Result<(), EngineError> {
    if is_opaque_reference(reference) {
        Ok(())
    } else {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "runtime reference must be opaque and authorized",
        )))
    }
}

fn is_opaque_reference(reference: &str) -> bool {
    if reference.len() < 4 || reference.len() > MAX_REFERENCE_BYTES {
        return false;
    }
    let Some((scheme, rest)) = reference.split_once(':') else {
        return false;
    };
    let mut scheme_characters = scheme.chars();
    let scheme_ok = scheme_characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && (2..=32).contains(&scheme.len())
        && scheme_characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '+' | '.' | '-')
        });
    let body = rest.strip_prefix("//").unwrap_or(rest);
    scheme_ok
        && scheme != "file"
        && !body.is_empty()
        && !reference.chars().any(|character| {
            character == '\\'
                || character.is_whitespace()
                || character.is_control()
                // Python's `\s`, which the contract grammar uses, also matches
                // the information separators U+001C to U+001F.
                || ('\u{1c}'..='\u{1f}').contains(&character)
        })
        && !reference.contains("%2E")
        && !reference.contains("%2e")
        && !has_dot_segment(reference)
}

/// A `.` or `..` segment: preceded by the start, `/` or `:`, and followed by
/// `/` or the end, as `(^|[/:])\.{1,2}(/|$)` in the contract grammar.
fn has_dot_segment(reference: &str) -> bool {
    let mut starts = std::iter::once(0).chain(
        reference
            .char_indices()
            .filter(|(_, character)| matches!(character, '/' | ':'))
            .map(|(index, _)| index + 1),
    );
    starts.any(|start| {
        let tail = reference.get(start..).unwrap_or_default();
        ["..", "."].iter().any(|dots| {
            tail.strip_prefix(dots)
                .is_some_and(|after| after.is_empty() || after.starts_with('/'))
        })
    })
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
        for accepted in [
            "artifact://tenant/item",
            "secret://rest/vector",
            "artifact:tenant-a/8d936f1d",
            "s3+v2://bucket/key.csv",
            "artifact://tenant/$HOME/report",
            "artifact://tenant/a..b",
        ] {
            assert!(validate_reference(accepted).is_ok(), "{accepted}");
        }
        for refused in [
            "",
            "../private/file",
            "./report.csv",
            "C:\\private\\file",
            "c:/private/file",
            "/etc/passwd",
            "dir/report.csv",
            "report.csv",
            "~/report.csv",
            "\\\\server\\share",
            "file:///etc/passwd",
            "FILE:x",
            "artifact://tenant/../other",
            "artifact://tenant/.",
            "artifact:..",
            "artifact://tenant/%2e%2e/x",
            "artifact://tenant/%2E",
            "artifact://tenant/a b",
            "artifact://tenant/a\u{1f}b",
            "artifact://tenant\\x",
            "artifact://",
            "artifact:",
            "a://x",
            "Artifact://x",
            "1rtifact://x",
        ] {
            assert!(validate_reference(refused).is_err(), "{refused:?}");
        }
        let too_long = format!("artifact://{}", "a".repeat(super::MAX_REFERENCE_BYTES));
        assert!(validate_reference(&too_long).is_err());
        let _resources = EmptyResources;
    }
}
