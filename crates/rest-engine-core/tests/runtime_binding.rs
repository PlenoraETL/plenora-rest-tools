use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use plenora_rest_core::{
    AuthConfig, CancellationToken, EXECUTION_REQUEST_CONTRACT, EXECUTION_RESULT_CONTRACT, Engine,
    EngineConfig, EngineError, FILE_TRANSFER_INPUT_CONTRACT, FILE_TRANSFER_RESULT_CONTRACT,
    RUNTIME_INTERFACE_CONTRACT, RuntimeBinding, RuntimeMessage, RuntimeMessageKind,
    RuntimeResources,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const MESSAGE_ID: &str = "11111111-1111-4111-8111-111111111111";
const CORRELATION_ID: &str = "22222222-2222-4222-8222-222222222222";

struct EmptyResources;

impl RuntimeResources for EmptyResources {
    fn resolve_credentials(&self, _reference: &str) -> Result<AuthConfig, EngineError> {
        Err(EngineError::InvalidInput(
            "credential reference was not configured".to_owned(),
        ))
    }

    fn resolve_artifact_source(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Err(EngineError::InvalidInput(
            "artifact source was not configured".to_owned(),
        ))
    }

    fn resolve_artifact_sink(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Err(EngineError::InvalidInput(
            "artifact sink was not configured".to_owned(),
        ))
    }
}

struct ArtifactResources {
    path: PathBuf,
}

impl RuntimeResources for ArtifactResources {
    fn resolve_credentials(&self, _reference: &str) -> Result<AuthConfig, EngineError> {
        Ok(AuthConfig::None)
    }

    fn resolve_artifact_source(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Ok(self.path.clone())
    }

    fn resolve_artifact_sink(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Ok(self.path.clone())
    }
}

fn runtime_request(url: &str) -> RuntimeMessage {
    RuntimeMessage {
        schema_version: 1,
        contract: RUNTIME_INTERFACE_CONTRACT.to_owned(),
        kind: RuntimeMessageKind::Request,
        content_type: "application/json".to_owned(),
        metadata: BTreeMap::from([
            ("plenora.message.id".to_owned(), MESSAGE_ID.to_owned()),
            (
                "plenora.trace.correlation_id".to_owned(),
                CORRELATION_ID.to_owned(),
            ),
            (
                "plenora.capability.name".to_owned(),
                "plenora.rest-tools".to_owned(),
            ),
            ("plenora.capability.version".to_owned(), "1".to_owned()),
            (
                "plenora.capability.operation".to_owned(),
                "rest.test".to_owned(),
            ),
            ("plenora.operation.version".to_owned(), "1".to_owned()),
            (
                "plenora.input.contract".to_owned(),
                EXECUTION_REQUEST_CONTRACT.to_owned(),
            ),
        ]),
        payload: json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET"
            },
            "input": {
                "params": {},
                "records": []
            }
        }),
    }
}

fn local_engine() -> Engine {
    Engine::new(EngineConfig {
        allow_private_networks: true,
        ..EngineConfig::default()
    })
}

async fn invoke_serialized<Resources: RuntimeResources>(
    binding: &RuntimeBinding<'_, '_, Resources>,
    request: RuntimeMessage,
    cancellation: CancellationToken,
) -> RuntimeMessage {
    let request_json = serde_json::to_string(&request).unwrap();
    let response_json = binding
        .invoke_json(&request_json, cancellation)
        .await
        .unwrap();
    serde_json::from_str(&response_json).unwrap()
}

#[tokio::test]
async fn runtime_success_preserves_trace_identity_and_contract() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request).await.unwrap();
        let body = br#"{"ok":true}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let response = invoke_serialized(
        &binding,
        runtime_request(&format!("http://{address}/")),
        CancellationToken::new(),
    )
    .await;
    server.await.unwrap();

    assert_eq!(response.kind, RuntimeMessageKind::Success);
    assert_eq!(response.payload["status"], "success");
    assert_eq!(
        response.metadata["plenora.output.contract"],
        EXECUTION_RESULT_CONTRACT
    );
    assert_eq!(
        response.metadata["plenora.trace.correlation_id"],
        CORRELATION_ID
    );
    assert_eq!(
        response.metadata["plenora.message.causation_id"],
        MESSAGE_ID
    );
    assert_ne!(response.metadata["plenora.message.id"], MESSAGE_ID);
}

#[tokio::test]
async fn runtime_rejects_contract_drift_and_inline_secrets_without_leaking_them() {
    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);

    let mut mismatch = runtime_request("http://127.0.0.1:9/");
    mismatch.payload["operation"] = Value::String("generate".to_owned());
    let mismatch_response = invoke_serialized(&binding, mismatch, CancellationToken::new()).await;
    assert_eq!(mismatch_response.kind, RuntimeMessageKind::Error);
    assert_eq!(mismatch_response.payload["code"], "INVALID_INPUT");
    assert_eq!(
        mismatch_response.metadata["plenora.output.contract"],
        "plenora-error-v1"
    );

    let mut secret = runtime_request("http://127.0.0.1:9/");
    secret.payload["connection"]["auth"] = json!({
        "type": "bearer",
        "token": "never-return-this-secret"
    });
    assert!(!format!("{secret:?}").contains("never-return-this-secret"));
    let secret_response = invoke_serialized(&binding, secret, CancellationToken::new()).await;
    let serialized = serde_json::to_string(&secret_response).unwrap();
    assert_eq!(secret_response.kind, RuntimeMessageKind::Error);
    assert!(!serialized.contains("never-return-this-secret"));
    assert_eq!(
        secret_response.metadata["plenora.trace.correlation_id"],
        CORRELATION_ID
    );
}

#[tokio::test]
async fn runtime_propagates_cancellation_and_engine_lifecycle() {
    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let cancelled = binding
        .invoke(runtime_request("http://127.0.0.1:9/"), cancellation)
        .await;
    assert_eq!(cancelled.kind, RuntimeMessageKind::Error);
    assert_eq!(cancelled.payload["code"], "CANCELLED");

    let mut expired_request = runtime_request("http://127.0.0.1:9/");
    expired_request.metadata.insert(
        "plenora.execution.deadline".to_owned(),
        "2000-01-01T00:00:00Z".to_owned(),
    );
    let expired = binding
        .invoke(expired_request, CancellationToken::new())
        .await;
    assert_eq!(expired.kind, RuntimeMessageKind::Error);
    assert_eq!(expired.payload["code"], "TIMEOUT");

    engine.close();
    let closed = binding
        .invoke(
            runtime_request("http://127.0.0.1:9/"),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(closed.kind, RuntimeMessageKind::Error);
    assert_eq!(closed.payload["code"], "ENGINE_CLOSED");
}

#[tokio::test]
async fn runtime_download_resolves_an_opaque_sink_without_exposing_its_path() {
    let body = b"runtime-artifact";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request).await.unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "plenora-rest-runtime-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    let sink = directory.join("download.bin");
    let resources = ArtifactResources { path: sink.clone() };
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_file_transfers: true,
        file_root: Some(directory.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    });
    let binding = RuntimeBinding::new(&engine, &resources);
    let mut request = runtime_request(&format!("http://{address}/"));
    request.metadata.insert(
        "plenora.capability.operation".to_owned(),
        "rest.download".to_owned(),
    );
    request.metadata.insert(
        "plenora.input.contract".to_owned(),
        FILE_TRANSFER_INPUT_CONTRACT.to_owned(),
    );
    request.payload = json!({
        "schema_version": 1,
        "operation": "download",
        "connection": {
            "url": format!("http://{address}/"),
            "method": "GET"
        },
        "input": {
            "file": {
                "artifact_sink": {
                    "reference": "artifact://tenant/export"
                }
            }
        }
    });

    let response = binding.invoke(request, CancellationToken::new()).await;
    server.await.unwrap();

    assert_eq!(response.kind, RuntimeMessageKind::Success);
    assert_eq!(
        response.metadata["plenora.output.contract"],
        FILE_TRANSFER_RESULT_CONTRACT
    );
    assert_eq!(
        response.payload["output"]["artifact_reference"],
        "artifact://tenant/export"
    );
    assert_eq!(fs::read(&sink).unwrap(), body);
    assert!(
        !serde_json::to_string(&response)
            .unwrap()
            .contains(&sink.to_string_lossy().to_string())
    );
    fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn runtime_json_envelope_is_strict() {
    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let mut request = serde_json::to_value(runtime_request("http://127.0.0.1:9/")).unwrap();
    request["unexpected"] = Value::Bool(true);

    let error = binding
        .invoke_json(&request.to_string(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, EngineError::InvalidInput(_)));
}
