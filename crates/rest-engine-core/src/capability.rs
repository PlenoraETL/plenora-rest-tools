use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

pub const COMPONENT_ID: &str = "plenora-rest-tools";
pub const CAPABILITY_SCHEMA_VERSION: u32 = 2;
pub const CAPABILITY_NAME: &str = "plenora.rest-tools";
pub const RUNTIME_BINDING_VERSION: u32 = 1;
pub const RUST_INTERFACE_CONTRACT: &str = "plenora-rust-public-v1";
pub const PYTHON_INTERFACE_CONTRACT: &str = "plenora-python-sdk-v1";
pub const RUNTIME_INTERFACE_CONTRACT: &str = "plenora-runtime-binding-v1";
pub const EXECUTION_REQUEST_CONTRACT: &str = "plenora-rest-execution-request-v1";
pub const EXECUTION_RESULT_CONTRACT: &str = "plenora-rest-execution-result-v1";
pub const FILE_TRANSFER_INPUT_CONTRACT: &str = "plenora-rest-file-transfer-input-v1";
pub const FILE_TRANSFER_RESULT_CONTRACT: &str = "plenora-rest-file-transfer-result-v1";
pub const CAPABILITY_ATTRIBUTES_CONTRACT: &str = "plenora-rest-capability-attributes-v1";

pub const REST_TEST: &str = "rest.test";
pub const REST_GENERATE: &str = "rest.generate";
pub const REST_ENRICH: &str = "rest.enrich";
pub const REST_DOWNLOAD: &str = "rest.download";
pub const REST_UPLOAD: &str = "rest.upload";

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CapabilityDocument {
    pub schema_version: u32,
    pub component: String,
    pub component_version: String,
    pub interfaces: Vec<CapabilityInterface>,
    pub operations: Vec<OperationCapability>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CapabilityInterface {
    pub kind: Surface,
    pub contract: String,
    pub version: u32,
    pub artifact: String,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Rust,
    PythonSdk,
    Runtime,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct OperationCapability {
    pub id: String,
    pub version: u32,
    pub status: CapabilityStatus,
    pub surfaces: Vec<Surface>,
    pub input: PayloadCapability,
    pub output: PayloadCapability,
    pub side_effect: SideEffect,
    pub controls: ExecutionControls,
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Available,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PayloadCapability {
    pub contract: String,
    pub content_types: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    Remote,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct ExecutionControls {
    pub cancellation: bool,
    pub deadline: bool,
    pub idempotency_key: bool,
}

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
                "circuit_breaker"
            ]),
        ),
        (
            "orchestration".to_owned(),
            json!(["pagination", "polling", "batch", "ordered_enrichment"]),
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
            idempotency_key: false,
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
                && !operation.controls.idempotency_key
        }));
    }
}
