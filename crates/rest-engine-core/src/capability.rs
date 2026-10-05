use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

/// Identifier of this component in the capability document (`component`).
pub const COMPONENT_ID: &str = "plenora-rest-tools";
/// Version of the capability document format (`schema_version`).
pub const CAPABILITY_SCHEMA_VERSION: u32 = 2;
/// Runtime capability name; a runtime request must carry it in
/// `plenora.capability.name`.
pub const CAPABILITY_NAME: &str = "plenora.rest-tools";
/// Version of the runtime envelope and of the runtime capability; a runtime
/// request must carry it as `schema_version` and `plenora.capability.version`.
pub const RUNTIME_BINDING_VERSION: u32 = 1;
/// Contract of the public Rust API of `plenora-rest-core`.
pub const RUST_INTERFACE_CONTRACT: &str = "plenora-rust-public-v1";
/// Contract of the public Python SDK (`plenora-rest`).
pub const PYTHON_INTERFACE_CONTRACT: &str = "plenora-python-sdk-v1";
/// Contract of the runtime envelope served by
/// [`RuntimeBinding`](crate::RuntimeBinding).
pub const RUNTIME_INTERFACE_CONTRACT: &str = "plenora-runtime-binding-v1";
/// Input contract of `rest.test`, `rest.generate` and `rest.enrich`: an
/// [`ExecutionRequest`](crate::ExecutionRequest) without file input.
pub const EXECUTION_REQUEST_CONTRACT: &str = "plenora-rest-execution-request-v1";
/// Output contract of `rest.test`, `rest.generate` and `rest.enrich`: an
/// [`ExecutionResult`](crate::ExecutionResult).
pub const EXECUTION_RESULT_CONTRACT: &str = "plenora-rest-execution-result-v1";
/// Input contract of `rest.download` and `rest.upload`: an execution request
/// whose `input.file` describes the transfer.
pub const FILE_TRANSFER_INPUT_CONTRACT: &str = "plenora-rest-file-transfer-input-v1";
/// Output contract of `rest.download` and `rest.upload`: an execution result
/// whose output describes the transferred file.
pub const FILE_TRANSFER_RESULT_CONTRACT: &str = "plenora-rest-file-transfer-result-v1";
/// Contract of the `attributes` map of every [`OperationCapability`].
pub const CAPABILITY_ATTRIBUTES_CONTRACT: &str = "plenora-rest-capability-attributes-v1";

/// Operation that sends one request and returns its decoded response as a
/// single JSON value.
pub const REST_TEST: &str = "rest.test";
/// Operation that maps the response into output records, following
/// pagination and polling when configured.
pub const REST_GENERATE: &str = "rest.generate";
/// Operation that issues requests driven by the input records (one per
/// record, or one per batch when `connection.batch` is enabled) and returns
/// the records enriched with the mapped response values, in input order.
pub const REST_ENRICH: &str = "rest.enrich";
/// Operation that downloads a response body to a file.
pub const REST_DOWNLOAD: &str = "rest.download";
/// Operation that uploads a file as the request body.
pub const REST_UPLOAD: &str = "rest.upload";

/// Self-description of this build: its public interfaces and the operations
/// it serves. Returned by [`capabilities`] and
/// [`Engine::capabilities`](crate::Engine::capabilities); it serializes to the
/// JSON capability document.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CapabilityDocument {
    /// Document format version, [`CAPABILITY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Component identifier, [`COMPONENT_ID`].
    pub component: String,
    /// Version of the `plenora-rest-core` crate that built the document.
    pub component_version: String,
    /// The Rust, Python SDK and runtime interfaces, in that order.
    pub interfaces: Vec<CapabilityInterface>,
    /// The five operations: `rest.test`, `rest.generate`, `rest.enrich`,
    /// `rest.download`, `rest.upload`.
    pub operations: Vec<OperationCapability>,
}

/// One public interface of the component.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CapabilityInterface {
    /// Which surface this interface is.
    pub kind: Surface,
    /// Contract identifier of the interface.
    pub contract: String,
    /// Major version of the contract.
    pub version: u32,
    /// Distributed artifact that provides it: the `plenora-rest-core` crate,
    /// the `plenora-rest` Python package, or the `plenora.rest-tools` runtime
    /// capability.
    pub artifact: String,
}

/// Public surface of the component, serialized in snake case.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// `rust`: the public Rust API of this crate.
    Rust,
    /// `python_sdk`: the Python SDK.
    PythonSdk,
    /// `runtime`: the runtime envelope binding.
    Runtime,
}

/// Description of one operation.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct OperationCapability {
    /// Operation identifier, such as `rest.generate`; the runtime selects it
    /// with `plenora.capability.operation`.
    pub id: String,
    /// Operation version; currently 1 for every operation.
    pub version: u32,
    /// Availability of the operation.
    pub status: CapabilityStatus,
    /// Surfaces that expose the operation; all three for every operation.
    pub surfaces: Vec<Surface>,
    /// Accepted input contract and content types.
    pub input: PayloadCapability,
    /// Produced output contract and content types.
    pub output: PayloadCapability,
    /// Effect of the operation outside the process.
    pub side_effect: SideEffect,
    /// Execution controls the operation honors.
    pub controls: ExecutionControls,
    /// Feature attributes under [`CAPABILITY_ATTRIBUTES_CONTRACT`]: `contract`,
    /// `http_methods`, `authentication`, `response_formats`, `resilience`,
    /// `orchestration` and `integrity` (`sha256`); file transfer operations
    /// add `direction` and `transfer`. Keys are sorted.
    pub attributes: BTreeMap<String, Value>,
}

/// Availability of an operation, serialized in snake case.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// `available`: the operation is implemented and served.
    Available,
}

/// Contract and content types of an operation input or output.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PayloadCapability {
    /// Contract identifier of the payload.
    pub contract: String,
    /// Accepted or produced content types; `application/json` for every
    /// operation.
    pub content_types: Vec<String>,
}

/// Side effect class of an operation, serialized in snake case.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// `remote`: the operation sends requests to remote services, which may
    /// change remote state.
    Remote,
}

/// Execution controls an operation honors.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct ExecutionControls {
    /// The operation can be cancelled with a
    /// [`CancellationToken`](crate::CancellationToken).
    pub cancellation: bool,
    /// The operation honors an RFC 3339 deadline.
    pub deadline: bool,
    /// The operation accepts an idempotency key.
    pub idempotency_key: bool,
}

/// Builds the capability document of this build. Deterministic: the same build
/// always returns the same document.
pub fn capabilities() -> CapabilityDocument {
    CapabilityDocument {
        schema_version: CAPABILITY_SCHEMA_VERSION,
        component: COMPONENT_ID.to_owned(),
        component_version: env!("CARGO_PKG_VERSION").to_owned(),
        interfaces: vec![
            CapabilityInterface {
                kind: Surface::Rust,
                contract: RUST_INTERFACE_CONTRACT.to_owned(),
                version: 1,
                artifact: "plenora-rest-core".to_owned(),
            },
            CapabilityInterface {
                kind: Surface::PythonSdk,
                contract: PYTHON_INTERFACE_CONTRACT.to_owned(),
                version: 1,
                artifact: "plenora-rest".to_owned(),
            },
            CapabilityInterface {
                kind: Surface::Runtime,
                contract: RUNTIME_INTERFACE_CONTRACT.to_owned(),
                version: RUNTIME_BINDING_VERSION,
                artifact: CAPABILITY_NAME.to_owned(),
            },
        ],
        operations: vec![
            operation(
                REST_TEST,
                EXECUTION_REQUEST_CONTRACT,
                EXECUTION_RESULT_CONTRACT,
                None,
            ),
            operation(
                REST_GENERATE,
                EXECUTION_REQUEST_CONTRACT,
                EXECUTION_RESULT_CONTRACT,
                None,
            ),
            operation(
                REST_ENRICH,
                EXECUTION_REQUEST_CONTRACT,
                EXECUTION_RESULT_CONTRACT,
                None,
            ),
            operation(
                REST_DOWNLOAD,
                FILE_TRANSFER_INPUT_CONTRACT,
                FILE_TRANSFER_RESULT_CONTRACT,
                Some("download"),
            ),
            operation(
                REST_UPLOAD,
                FILE_TRANSFER_INPUT_CONTRACT,
                FILE_TRANSFER_RESULT_CONTRACT,
                Some("upload"),
            ),
        ],
    }
}

fn operation(
    id: &str,
    input_contract: &str,
    output_contract: &str,
    direction: Option<&str>,
) -> OperationCapability {
    let mut attributes = BTreeMap::from([
        (
            "contract".to_owned(),
            Value::String(CAPABILITY_ATTRIBUTES_CONTRACT.to_owned()),
        ),
        (
            "http_methods".to_owned(),
            json!([
                "GET",
                "HEAD",
                "POST",
                "PUT",
                "PATCH",
                "DELETE",
                "OPTIONS",
                "custom_allowlist"
            ]),
        ),
        (
            "authentication".to_owned(),
            json!([
                "none",
                "bearer",
                "api_key",
                "basic_auth",
                "oauth2_client_credentials",
                "oauth2_password",
                "arcgis_token"
            ]),
        ),
        (
            "response_formats".to_owned(),
            json!(["json", "csv", "xml", "ndjson", "text", "binary"]),
        ),
        (
            "resilience".to_owned(),
            json!([
                "retry",
                "retry_after",
                "rate_limit",
                "cache",
                "cookies",
                "circuit_breaker",
                "idempotency_key"
            ]),
        ),
        (
            "orchestration".to_owned(),
            json!([
                "pagination",
                "polling",
                "async_job_recovery",
                "remote_cancel",
                "batch",
                "ordered_enrichment"
            ]),
        ),
        ("integrity".to_owned(), Value::String("sha256".to_owned())),
    ]);
    if let Some(direction) = direction {
        attributes.insert("direction".to_owned(), Value::String(direction.to_owned()));
        attributes.insert(
            "transfer".to_owned(),
            json!(["bounded", "streaming", "runtime_artifact_reference"]),
        );
    }
    OperationCapability {
        id: id.to_owned(),
        version: 1,
        status: CapabilityStatus::Available,
        surfaces: vec![Surface::Rust, Surface::PythonSdk, Surface::Runtime],
        input: PayloadCapability {
            contract: input_contract.to_owned(),
            content_types: vec!["application/json".to_owned()],
        },
        output: PayloadCapability {
            contract: output_contract.to_owned(),
            content_types: vec!["application/json".to_owned()],
        },
        side_effect: SideEffect::Remote,
        controls: ExecutionControls {
            cancellation: true,
            deadline: true,
            idempotency_key: true,
        },
        attributes,
    }
}

#[cfg(test)]
mod tests {
    use super::{CAPABILITY_ATTRIBUTES_CONTRACT, capabilities};

    #[test]
    fn capability_document_is_complete_and_truthful() {
        let document = capabilities();
        assert_eq!(document.operations.len(), 5);
        assert!(document.operations.iter().all(|operation| {
            operation.attributes["contract"] == CAPABILITY_ATTRIBUTES_CONTRACT
                && operation.controls.cancellation
                && operation.controls.deadline
                && operation.controls.idempotency_key
        }));
    }
}
