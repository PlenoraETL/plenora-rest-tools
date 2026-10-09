use std::{collections::BTreeMap, fmt, path::PathBuf};

use crate::error::ErrorDetail;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::{
    AuthConfig, CAPABILITY_NAME, CancellationToken, Engine, EngineError, ErrorCategory,
    ErrorPayload, ErrorPhase, ExecutionControl, ExecutionOperation, ExecutionRequest,
    ExecutionResult, ExecutionStatus, FILE_TRANSFER_INPUT_CONTRACT, FILE_TRANSFER_RESULT_CONTRACT,
    IdempotencyLocation, ParameterLocation, REST_DOWNLOAD, REST_ENRICH, REST_GENERATE, REST_TEST,
    REST_UPLOAD, RUNTIME_BINDING_VERSION, RemoteEffect,
    capability::{
        EXECUTION_REQUEST_CONTRACT, EXECUTION_RESULT_CONTRACT, RUNTIME_INTERFACE_CONTRACT,
    },
    control::is_utc_deadline,
    engine::{is_disallowed_idempotency_header, is_sensitive_header_name},
    error::RetryAdvice,
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
    /// (RFC 3339 in UTC, not together with `options.deadline` in the
    /// payload) and `plenora.execution.idempotency_key`; keys the binding does
    /// not reserve are ignored. A response carries a new `plenora.message.id`,
    /// `plenora.output.contract`, and the request's message id as
    /// `plenora.message.causation_id`, correlation id, operation and operation
    /// version, each only when the request's value is canonical.
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
        let (mut execution, operation, output_contract, control) =
            match self.prepare_request(&request, cancellation) {
                Ok(prepared) => prepared,
                Err(error) => return error_message(&request, error),
            };
        // Checked before any resource is resolved: an expired deadline refuses
        // the message with nothing done, local or remote.
        if control.deadline_expired() {
            return error_message(&request, EngineError::DeadlineExpired.payload());
        }
        if let Err(error) = resolve_runtime_inputs(&mut execution, operation, self.resources) {
            return error_message(&request, error.payload());
        }
        let result = self.engine.execute_with_control(execution, control).await;
        if result.status == ExecutionStatus::Failed {
            let mut error = result
                .errors
                .first()
                .map(execution_error_payload)
                .unwrap_or_else(|| {
                    EngineError::Runtime(ErrorDetail::from("execution failed without an error"))
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

    /// Serialized form of [`RuntimeBinding::invoke`].
    ///
    /// Text that is not a JSON envelope at all has nothing to answer to and is
    /// an `Err`. An envelope whose metadata carries a value that is not a
    /// string (a `null` idempotency key, a numeric version) is answered with a
    /// `protocol` refusal: the metadata of the binding are strings, and a
    /// value of another type is neither absent nor valid.
    pub async fn invoke_json(
        &self,
        request_json: &str,
        cancellation: CancellationToken,
    ) -> Result<String, EngineError> {
        // The envelope is read as a typed message first, so a duplicate
        // member or an unknown one is refused exactly as before. Only a
        // document that fails that because some metadata value is not a
        // string is read again, with metadata of any JSON type, to answer it
        // with a protocol refusal (RT-017): the metadata of the binding are
        // strings, and a `null` or a number is neither absent nor valid.
        let typed = serde_json::from_str::<RuntimeMessage>(request_json);
        let response = match typed {
            Ok(request) => self.invoke(request, cancellation).await,
            Err(typed_error) => {
                let not_an_envelope = EngineError::InvalidInput(ErrorDetail::at(
                    "request is not valid JSON for the contract",
                    typed_error.line(),
                    typed_error.column(),
                ));
                let Ok(raw) = serde_json::from_str::<RawEnvelope>(request_json) else {
                    return Err(not_an_envelope);
                };
                if raw.metadata.values().all(Value::is_string) {
                    return Err(not_an_envelope);
                }
                let envelope = RuntimeMessage {
                    schema_version: raw.schema_version,
                    contract: raw.contract,
                    kind: raw.kind,
                    content_type: raw.content_type,
                    metadata: raw
                        .metadata
                        .into_iter()
                        .filter_map(|(key, value)| match value {
                            Value::String(text) => Some((key, text)),
                            _ => None,
                        })
                        .collect(),
                    payload: raw.payload,
                };
                error_message(
                    &envelope,
                    refusal(
                        ErrorCategory::Protocol,
                        "runtime metadata values must be strings",
                    ),
                )
            }
        };
        serde_json::to_string(&response).map_err(|_| {
            EngineError::Runtime(ErrorDetail::from("response could not be serialized"))
        })
    }

    fn prepare_request(
        &self,
        message: &RuntimeMessage,
        cancellation: CancellationToken,
    ) -> Result<
        (
            ExecutionRequest,
            ExecutionOperation,
            &'static str,
            ExecutionControl,
        ),
        ErrorPayload,
    > {
        // RT-018: the category of a refusal is the first that applies,
        // `protocol` (every reserved value checked against its grammar), then
        // `unsupported` (well-formed but not advertised), then `timeout`.
        validate_grammar(message)?;
        let (operation, output_contract) = validate_support(message)?;
        let metadata_deadline = message.metadata.get(DEADLINE_KEY);
        if let Some(deadline) = metadata_deadline {
            let control = ExecutionControl::default()
                .with_deadline(deadline)
                .map_err(|error| error.payload())?;
            if control.deadline_expired() {
                return Err(EngineError::DeadlineExpired.payload());
            }
        }
        // A JSON value has no line or column, and serde's message can quote
        // the payload, so only the fact is kept.
        let mut execution = serde_json::from_value::<ExecutionRequest>(message.payload.clone())
            .map_err(|_| {
                EngineError::InvalidInput(ErrorDetail::from(
                    "runtime payload does not match the execution request contract",
                ))
                .payload()
            })?;
        if execution.options.idempotency_key.is_some() {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "runtime idempotency key must use plenora.execution.idempotency_key metadata",
            ))
            .payload());
        }
        if execution.operation != operation {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "runtime selector and payload operation differ",
            ))
            .payload());
        }
        // One deadline, from one channel: with both present, neither wins
        // silently, even when the two values are equal.
        let deadline = match (metadata_deadline, execution.options.deadline.as_deref()) {
            (Some(_), Some(_)) => {
                return Err(refusal(
                    ErrorCategory::InvalidConfiguration,
                    "runtime deadline is given both in metadata and in the payload",
                ));
            }
            (Some(metadata), None) => Some(metadata.clone()),
            (None, payload) => payload.map(str::to_owned),
        };
        let control = ExecutionControl::new(cancellation)
            .with_optional_deadline(deadline.as_deref())
            .map_err(|error| error.payload())?;
        if let Some(key) = message.metadata.get(IDEMPOTENCY_KEY) {
            execution.options.idempotency_key = Some(key.clone());
        }
        Ok((execution, operation, output_contract, control))
    }
}

const MESSAGE_ID_KEY: &str = "plenora.message.id";
const CAUSATION_KEY: &str = "plenora.message.causation_id";
const CORRELATION_KEY: &str = "plenora.trace.correlation_id";
const CAPABILITY_NAME_KEY: &str = "plenora.capability.name";
const CAPABILITY_VERSION_KEY: &str = "plenora.capability.version";
const OPERATION_KEY: &str = "plenora.capability.operation";
const OPERATION_VERSION_KEY: &str = "plenora.operation.version";
const INPUT_CONTRACT_KEY: &str = "plenora.input.contract";
const OUTPUT_CONTRACT_KEY: &str = "plenora.output.contract";
const DEADLINE_KEY: &str = "plenora.execution.deadline";
const IDEMPOTENCY_KEY: &str = "plenora.execution.idempotency_key";

/// A refusal before invocation: phase `validate`, no remote effect, and
/// `never` retry, since the same message fails the same way again.
///
/// The category follows RT-018 (Runtime Binding 1.0, plenora-contracts
/// v1.1.0): `unsupported` for a well-formed value this component does not
/// advertise, `protocol` for a value that is absent, malformed or not
/// canonical; the three other axes are those of RT-016.
fn refusal(category: ErrorCategory, message: &'static str) -> ErrorPayload {
    let code = match category {
        ErrorCategory::Unsupported => "RUNTIME_UNSUPPORTED",
        ErrorCategory::Protocol => "RUNTIME_PROTOCOL_VIOLATION",
        _ => "INVALID_INPUT",
    };
    ErrorPayload {
        category,
        phase: ErrorPhase::Validate,
        remote_effect: RemoteEffect::None,
        retry: RetryAdvice::NEVER,
        code: code.to_owned(),
        message: message.to_owned(),
        details: BTreeMap::new(),
    }
}

/// A runtime envelope whose metadata values may be of any JSON type, read
/// only to answer one that carries a non-string value. Unknown and duplicate
/// members are refused as for [`RuntimeMessage`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvelope {
    schema_version: u32,
    contract: String,
    kind: RuntimeMessageKind,
    content_type: String,
    metadata: BTreeMap<String, Value>,
    payload: Value,
}

/// `tchar` of RFC 9110 §5.6.2.
fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// `type "/" subtype *( OWS ";" OWS parameter )` of RFC 9110 §8.3.1, with a
/// parameter value that is a token or a quoted string. `application//json`,
/// `application/` or a stray `;` are malformed (`protocol`); a well-formed
/// type the component does not advertise is `unsupported`.
fn is_media_type(value: &str) -> bool {
    let mut parts = value.split(';');
    let essence = parts.next().unwrap_or_default();
    let Some((kind, subtype)) = essence.split_once('/') else {
        return false;
    };
    is_token(kind)
        && is_token(subtype)
        && parts.all(|parameter| {
            let parameter = parameter.trim_matches([' ', '\t']);
            parameter.split_once('=').is_some_and(|(name, value)| {
                is_token(name)
                    && (is_token(value)
                        || value
                            .strip_prefix('"')
                            .and_then(|inner| inner.strip_suffix('"'))
                            .is_some_and(|inner| {
                                !inner.contains('"')
                                    && inner
                                        .bytes()
                                        .all(|byte| byte == b'\t' || (0x20..0x7f).contains(&byte))
                            }))
            })
        })
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|parsed| parsed.hyphenated().to_string() == value)
}

/// `^[1-9][0-9]*$`, the version grammar of the runtime vectors.
fn is_canonical_version(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|first| matches!(first, b'1'..=b'9'))
        && bytes.all(|byte| byte.is_ascii_digit())
}

/// `^[a-z][a-z0-9_-]*(\.[a-z][a-z0-9_-]*)+$`, the operation grammar.
fn is_canonical_operation(value: &str) -> bool {
    let mut segments = value.split('.');
    let mut count = 0_usize;
    let all = segments.all(|segment| {
        count += 1;
        let mut bytes = segment.bytes();
        bytes.next().is_some_and(|first| first.is_ascii_lowercase())
            && bytes.all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
    });
    all && count >= 2
}

/// `^plenora-[a-z0-9-]+-v[1-9][0-9]*$`, the contract identifier grammar.
fn is_canonical_contract(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("plenora-") else {
        return false;
    };
    let Some((name, version)) = rest.rsplit_once("-v") else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && is_canonical_version(version)
}

/// `^plenora\.[a-z][a-z0-9-]*-tools$`, the capability name grammar.
fn is_canonical_capability_name(value: &str) -> bool {
    value
        .strip_prefix("plenora.")
        .and_then(|rest| rest.strip_suffix("-tools"))
        .is_some_and(|stem| {
            let mut bytes = stem.bytes();
            bytes.next().is_some_and(|first| first.is_ascii_lowercase())
                && bytes
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

/// A reserved key, its grammar, and the refusal text when it does not match.
type Grammar = (&'static str, fn(&str) -> bool, &'static str);

/// Every reserved value against its grammar (RT-017): absent where
/// required, malformed or not canonical is `protocol`, and a value is never
/// normalized into the grammar. Keys the binding does not reserve, including
/// unknown `plenora.*` ones, are ignored as optional members.
fn validate_grammar(message: &RuntimeMessage) -> Result<(), ErrorPayload> {
    let protocol = |text: &'static str| Err(refusal(ErrorCategory::Protocol, text));
    if message.kind != RuntimeMessageKind::Request {
        return protocol("runtime invocation requires a request envelope");
    }
    if !is_media_type(&message.content_type) {
        return protocol("runtime request content type is malformed");
    }
    if !message.payload.is_object() {
        return protocol("runtime request payload must be a JSON object");
    }
    for key in [MESSAGE_ID_KEY, CORRELATION_KEY] {
        if !message
            .metadata
            .get(key)
            .is_some_and(|value| is_canonical_uuid(value))
        {
            return protocol("runtime identity metadata must be a canonical UUID");
        }
    }
    if message
        .metadata
        .get(CAUSATION_KEY)
        .is_some_and(|value| !is_canonical_uuid(value))
    {
        return protocol("runtime causation metadata must be a canonical UUID");
    }
    let grammars: [Grammar; 5] = [
        (
            CAPABILITY_NAME_KEY,
            is_canonical_capability_name,
            "runtime capability name is missing or malformed",
        ),
        (
            CAPABILITY_VERSION_KEY,
            is_canonical_version,
            "runtime capability version is missing or not canonical",
        ),
        (
            OPERATION_KEY,
            is_canonical_operation,
            "runtime operation is missing or malformed",
        ),
        (
            OPERATION_VERSION_KEY,
            is_canonical_version,
            "runtime operation version is missing or not canonical",
        ),
        (
            INPUT_CONTRACT_KEY,
            is_canonical_contract,
            "runtime input contract is missing or malformed",
        ),
    ];
    for (key, well_formed, text) in grammars {
        if !message
            .metadata
            .get(key)
            .is_some_and(|value| well_formed(value))
        {
            return protocol(text);
        }
    }
    if message
        .metadata
        .get(DEADLINE_KEY)
        .is_some_and(|value| !is_utc_deadline(value))
    {
        return protocol("runtime deadline must be an RFC 3339 timestamp in UTC");
    }
    if let Some(key) = message.metadata.get(IDEMPOTENCY_KEY)
        && (key.is_empty()
            || key.len() > 255
            || !key.bytes().all(|byte| matches!(byte, 0x21..=0x7e)))
    {
        return protocol("runtime idempotency key must contain 1 to 255 visible ASCII bytes");
    }
    Ok(())
}

/// Well-formed values that do not select one advertised runtime operation
/// (RT-018 step 2): `unsupported`.
fn validate_support(
    message: &RuntimeMessage,
) -> Result<(ExecutionOperation, &'static str), ErrorPayload> {
    let unsupported = |text: &'static str| Err(refusal(ErrorCategory::Unsupported, text));
    let value = |key: &str| message.metadata.get(key).map_or("", String::as_str);
    if message.schema_version != RUNTIME_BINDING_VERSION {
        return unsupported("runtime envelope schema version is unsupported");
    }
    if message.contract != RUNTIME_INTERFACE_CONTRACT && message.contract != RUNTIME_VECTOR_CONTRACT
    {
        return unsupported("runtime envelope contract is unsupported");
    }
    if message.content_type != JSON_CONTENT_TYPE {
        return unsupported("runtime request content type is not advertised");
    }
    if value(CAPABILITY_NAME_KEY) != CAPABILITY_NAME {
        return unsupported("runtime capability name is not plenora.rest-tools");
    }
    if value(CAPABILITY_VERSION_KEY) != RUNTIME_BINDING_VERSION.to_string() {
        return unsupported("runtime capability version is unsupported");
    }
    let Some((operation, input_contract, output_contract)) =
        operation_contracts(value(OPERATION_KEY))
    else {
        return unsupported("runtime operation is not advertised");
    };
    if value(OPERATION_VERSION_KEY) != "1" {
        return unsupported("runtime operation version is not advertised");
    }
    if value(INPUT_CONTRACT_KEY) != input_contract {
        return unsupported("runtime input contract does not match the operation");
    }
    Ok((operation, output_contract))
}

fn operation_contracts(id: &str) -> Option<(ExecutionOperation, &'static str, &'static str)> {
    match id {
        REST_TEST => Some((
            ExecutionOperation::Test,
            EXECUTION_REQUEST_CONTRACT,
            EXECUTION_RESULT_CONTRACT,
        )),
        REST_GENERATE => Some((
            ExecutionOperation::Generate,
            EXECUTION_REQUEST_CONTRACT,
            EXECUTION_RESULT_CONTRACT,
        )),
        REST_ENRICH => Some((
            ExecutionOperation::Enrich,
            EXECUTION_REQUEST_CONTRACT,
            EXECUTION_RESULT_CONTRACT,
        )),
        REST_DOWNLOAD => Some((
            ExecutionOperation::Download,
            FILE_TRANSFER_INPUT_CONTRACT,
            FILE_TRANSFER_RESULT_CONTRACT,
        )),
        REST_UPLOAD => Some((
            ExecutionOperation::Upload,
            FILE_TRANSFER_INPUT_CONTRACT,
            FILE_TRANSFER_RESULT_CONTRACT,
        )),
        _ => None,
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
pub(crate) fn validate_reference(reference: &str) -> Result<(), EngineError> {
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

/// Metadata of a response, success or error.
///
/// The message identity is always new. The causation is the request's message
/// identity, and correlation, operation and operation version are the
/// request's, each copied byte for byte only when it is canonical: a value
/// that is absent or not canonical is left out, never normalized, never
/// invented (R2), so a refusal never puts an invalid value under a reserved
/// key.
fn response_metadata(request: &RuntimeMessage, output_contract: &str) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::from([
        (MESSAGE_ID_KEY.to_owned(), Uuid::new_v4().to_string()),
        (OUTPUT_CONTRACT_KEY.to_owned(), output_contract.to_owned()),
    ]);
    let copy = |metadata: &mut BTreeMap<String, String>,
                from: &str,
                to: &str,
                canonical: fn(&str) -> bool| {
        if let Some(value) = request.metadata.get(from).filter(|value| canonical(value)) {
            metadata.insert(to.to_owned(), value.clone());
        }
    };
    copy(
        &mut metadata,
        CORRELATION_KEY,
        CORRELATION_KEY,
        is_canonical_uuid,
    );
    copy(
        &mut metadata,
        MESSAGE_ID_KEY,
        CAUSATION_KEY,
        is_canonical_uuid,
    );
    copy(
        &mut metadata,
        OPERATION_KEY,
        OPERATION_KEY,
        is_canonical_operation,
    );
    copy(
        &mut metadata,
        OPERATION_VERSION_KEY,
        OPERATION_VERSION_KEY,
        is_canonical_version,
    );
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
