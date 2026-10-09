use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use plenora_rest_core::{
    ASYNC_JOB_RECOVERY_CONTRACT, AuthConfig, CancellationToken, EXECUTION_REQUEST_CONTRACT,
    EXECUTION_RESULT_CONTRACT, Engine, EngineConfig, EngineError, FILE_TRANSFER_INPUT_CONTRACT,
    FILE_TRANSFER_RESULT_CONTRACT, RUNTIME_INTERFACE_CONTRACT, RuntimeBinding, RuntimeMessage,
    RuntimeMessageKind, RuntimeResources,
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
            "credential reference was not configured".into(),
        ))
    }

    fn resolve_artifact_source(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Err(EngineError::InvalidInput(
            "artifact source was not configured".into(),
        ))
    }

    fn resolve_artifact_sink(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Err(EngineError::InvalidInput(
            "artifact sink was not configured".into(),
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
async fn runtime_idempotency_metadata_reaches_http_and_conflicts_fail_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let read = stream.read(&mut request).await.unwrap();
        let body = br#"{"ok":true}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        stream.shutdown().await.unwrap();
        String::from_utf8_lossy(&request[..read]).into_owned()
    });

    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let mut request = runtime_request(&format!("http://{address}/"));
    request.metadata.insert(
        "plenora.execution.idempotency_key".to_owned(),
        "runtime-job-42".to_owned(),
    );
    let first = binding
        .invoke(request.clone(), CancellationToken::new())
        .await;
    let observed = server.await.unwrap();

    assert_eq!(first.kind, RuntimeMessageKind::Success);
    assert!(
        observed
            .to_ascii_lowercase()
            .contains("idempotency-key: runtime-job-42")
    );

    request.payload["input"]["params"]["different"] = Value::Bool(true);
    let conflict = binding.invoke(request, CancellationToken::new()).await;
    assert_eq!(conflict.kind, RuntimeMessageKind::Error);
    assert_eq!(conflict.payload["code"], "IDEMPOTENCY_CONFLICT");
}

#[tokio::test]
async fn runtime_polling_failure_exposes_a_bounded_recovery_handle() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for (status, body, location) in [
            (
                202,
                br#"{"id":"runtime-job-7","status":"queued"}"#.as_slice(),
                Some("/jobs/runtime-job-7"),
            ),
            (200, br#"{"status":"running"}"#.as_slice(), None),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            let location = location
                .map(|value| format!("Location: {value}\r\n"))
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });

    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let mut request = runtime_request(&format!("http://{address}/submit"));
    request.payload["connection"] = json!({
        "url": format!("http://{address}/submit"),
        "method": "POST",
        "polling": {
            "status_path": "status",
            "interval_ms": 0,
            "max_attempts": 1
        }
    });

    let response = binding.invoke(request, CancellationToken::new()).await;
    server.await.unwrap();

    assert_eq!(response.kind, RuntimeMessageKind::Error);
    assert_eq!(response.payload["code"], "POLLING_TIMEOUT");
    assert_eq!(
        response.payload["details"]["async_jobs"][0]["contract"],
        ASYNC_JOB_RECOVERY_CONTRACT
    );
    assert_eq!(
        response.payload["details"]["async_jobs"][0]["job_id"],
        "runtime-job-7"
    );
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
async fn runtime_rejects_secrets_smuggled_through_the_parameter_list() {
    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);

    // A parameter with `location: "header"` or `location: "cookie"` becomes an
    // HTTP header later on, so it is the same credential channel that
    // `connection.headers` already blocks.
    let smuggled = [
        json!({
            "name": "Authorization",
            "mode": "fixed",
            "value": "Bearer never-return-this-secret",
            "location": "header"
        }),
        json!({
            "name": "X-Auth-Token",
            "mode": "fixed",
            "value": "never-return-this-secret",
            "location": "header"
        }),
        json!({
            "name": "session",
            "mode": "fixed",
            "value": "never-return-this-secret",
            "location": "cookie"
        }),
    ];

    for parameter in smuggled {
        let mut request = runtime_request("http://127.0.0.1:9/");
        request.payload["connection"]["parameters"] = json!([parameter]);
        let response = invoke_serialized(&binding, request, CancellationToken::new()).await;
        let serialized = serde_json::to_string(&response).unwrap();
        assert_eq!(response.kind, RuntimeMessageKind::Error);
        assert_eq!(response.payload["code"], "INVALID_INPUT");
        assert!(
            !serialized.contains("never-return-this-secret"),
            "rejection must not echo the secret back: {serialized}"
        );
    }

    // A non-credential header parameter stays allowed.
    //
    // The classification must not swallow ordinary headers: this one has to
    // reach the transport. A listener bound by the test proves it, without
    // depending on a well-known port happening to be closed.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let observed = tokio::spawn(async move {
        // Bounded so a regression that rejects the request before the network
        // fails the test instead of hanging it, and read in a loop because one
        // TCP read is not guaranteed to carry the whole header block.
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("the request never reached the transport")
            .unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 512];
            let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
                .await
                .expect("the request headers never arrived")
                .unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let body = br#"{"ok":true}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        stream.shutdown().await.unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });

    let mut allowed = runtime_request(&format!("http://{address}/"));
    allowed.payload["connection"]["parameters"] = json!([{
        "name": "X-Request-Id",
        "mode": "fixed",
        "value": "abc",
        "location": "header"
    }]);
    let response = invoke_serialized(&binding, allowed, CancellationToken::new()).await;
    let request = observed.await.unwrap().to_ascii_lowercase();

    assert_eq!(response.kind, RuntimeMessageKind::Success);
    assert!(
        request.contains("x-request-id: abc"),
        "a non-credential header parameter must reach the wire: {request}"
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
    assert_eq!(expired.payload["code"], "DEADLINE_EXPIRED");
    assert_eq!(expired.payload["phase"], "validate");
    assert_eq!(expired.payload["remote_effect"], "none");

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

#[tokio::test]
async fn runtime_requests_carry_cookie_session_handles_opened_by_the_host() {
    // The host owns the engine and its sessions; the runtime payload only
    // names a handle. A closed session is refused before any network activity.
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        ..EngineConfig::default()
    });
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let session = engine.open_cookie_session().await.unwrap();
    engine.close_cookie_session(&session).await.unwrap();

    let mut request = runtime_request("http://127.0.0.1:9/");
    request.payload["connection"]["cookies"] = json!({"session": session});
    let response = invoke_serialized(&binding, request, CancellationToken::new()).await;
    assert_eq!(response.kind, RuntimeMessageKind::Error);
    assert_eq!(response.payload["code"], "POLICY_VIOLATION");

    let mut malformed = runtime_request("http://127.0.0.1:9/");
    malformed.payload["connection"]["cookies"] = json!({"enabled": true, "jar_id": "tenant"});
    let response = invoke_serialized(&binding, malformed, CancellationToken::new()).await;
    assert_eq!(response.kind, RuntimeMessageKind::Error);
    assert_eq!(response.payload["code"], "INVALID_INPUT");
}

#[tokio::test]
async fn runtime_honours_the_deadline_carried_in_the_metadata() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 2048];
        while stream.read(&mut buffer).await.unwrap_or(0) > 0 {}
    });
    let soon = SystemTime::now() + Duration::from_millis(300);
    let seconds = soon.duration_since(UNIX_EPOCH).unwrap().as_secs() + 1;
    let at = time::OffsetDateTime::from_unix_timestamp(i64::try_from(seconds).unwrap()).unwrap();
    let rfc3339 = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    );
    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let mut request = runtime_request(&format!("http://{address}/"));
    request
        .metadata
        .insert("plenora.execution.deadline".to_owned(), rfc3339);
    let started = std::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        binding.invoke(request, CancellationToken::new()),
    )
    .await
    .expect("the metadata deadline ends the call");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(response.kind, RuntimeMessageKind::Error);
    assert_eq!(response.payload["code"], "TIMEOUT");
    server.abort();
}

/// Resources that must never be asked anything: every probe below is refused
/// before invocation.
struct UntouchedResources(std::sync::Mutex<usize>);

impl RuntimeResources for UntouchedResources {
    fn resolve_credentials(&self, _reference: &str) -> Result<AuthConfig, EngineError> {
        *self.0.lock().unwrap() += 1;
        Ok(AuthConfig::None)
    }

    fn resolve_artifact_source(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        *self.0.lock().unwrap() += 1;
        Err(EngineError::InvalidInput("unexpected".into()))
    }

    fn resolve_artifact_sink(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        *self.0.lock().unwrap() += 1;
        Err(EngineError::InvalidInput("unexpected".into()))
    }
}

/// Invokes `request` with a credential reference, so a request that got as
/// far as resource resolution would be counted.
async fn refused(request: RuntimeMessage) -> RuntimeMessage {
    let engine = local_engine();
    let resources = UntouchedResources(std::sync::Mutex::new(0));
    let binding = RuntimeBinding::new(&engine, &resources);
    let response = invoke_serialized(&binding, request, CancellationToken::new()).await;
    assert_eq!(
        *resources.0.lock().unwrap(),
        0,
        "a resource was resolved: {:?}",
        response.payload
    );
    response
}

fn with_credential(mut request: RuntimeMessage) -> RuntimeMessage {
    request.payload["connection"]["credential_ref"] = json!("secret://rest/vector");
    request
}

fn assert_refusal(response: &RuntimeMessage, category: &str, label: &str) {
    assert_eq!(response.kind, RuntimeMessageKind::Error, "{label}");
    assert_eq!(
        response.content_type, "application/vnd.plenora.error+json",
        "{label}"
    );
    assert_eq!(
        response.metadata["plenora.output.contract"], "plenora-error-v1",
        "{label}"
    );
    assert_eq!(
        response.payload["category"], category,
        "{label}: {:?}",
        response.payload
    );
    assert_eq!(response.payload["phase"], "validate", "{label}");
    assert_eq!(response.payload["remote_effect"], "none", "{label}");
    assert_eq!(
        response.payload["retry"],
        json!({"kind": "never"}),
        "{label}"
    );
}

#[tokio::test]
async fn routing_refusals_follow_the_shared_matrix() {
    // (key, value or None for absent, expected category)
    let cases: [(&str, Option<&str>, &str); 17] = [
        (
            "plenora.capability.name",
            Some("plenora.storage-tools"),
            "unsupported",
        ),
        ("plenora.capability.name", Some("Plenora.REST"), "protocol"),
        ("plenora.capability.name", None, "protocol"),
        ("plenora.capability.version", Some("2"), "unsupported"),
        ("plenora.capability.version", Some("01"), "protocol"),
        ("plenora.capability.version", None, "protocol"),
        (
            "plenora.capability.operation",
            Some("rest.delete"),
            "unsupported",
        ),
        (
            "plenora.capability.operation",
            Some("REST.test"),
            "protocol",
        ),
        ("plenora.capability.operation", None, "protocol"),
        ("plenora.operation.version", Some("2"), "unsupported"),
        ("plenora.operation.version", Some("01"), "protocol"),
        ("plenora.operation.version", Some("+1"), "protocol"),
        ("plenora.operation.version", Some(" 1"), "protocol"),
        ("plenora.operation.version", Some("uno"), "protocol"),
        (
            "plenora.input.contract",
            Some("plenora-rest-file-transfer-input-v1"),
            "unsupported",
        ),
        ("plenora.input.contract", Some("rest input"), "protocol"),
        ("plenora.input.contract", None, "protocol"),
    ];
    for (key, value, category) in cases {
        let mut request = with_credential(runtime_request("http://127.0.0.1:9/"));
        match value {
            Some(value) => request.metadata.insert(key.to_owned(), value.to_owned()),
            None => request.metadata.remove(key),
        };
        let label = format!("{key}={value:?}");
        let response = refused(request).await;
        assert_refusal(&response, category, &label);
        // R2: a routing value is reflected only when canonical, byte for byte.
        let version = response.metadata.get("plenora.operation.version");
        match (key, value) {
            ("plenora.operation.version", Some("2")) => {
                assert_eq!(version.map(String::as_str), Some("2"), "{label}");
            }
            ("plenora.operation.version", _) => assert_eq!(version, None, "{label}"),
            _ => assert_eq!(version.map(String::as_str), Some("1"), "{label}"),
        }
        let operation = response.metadata.get("plenora.capability.operation");
        match (key, value) {
            ("plenora.capability.operation", Some("rest.delete")) => {
                assert_eq!(
                    operation.map(String::as_str),
                    Some("rest.delete"),
                    "{label}"
                );
            }
            ("plenora.capability.operation", _) => assert_eq!(operation, None, "{label}"),
            _ => assert_eq!(operation.map(String::as_str), Some("rest.test"), "{label}"),
        }
        assert_eq!(
            response.metadata["plenora.trace.correlation_id"], CORRELATION_ID,
            "{label}"
        );
    }
}

#[tokio::test]
async fn non_canonical_identities_are_refused_and_never_reflected() {
    // MESSAGE_ID has no letters, so its uppercase form would be identical.
    let upper = "AAAAAAAA-1111-4111-8111-111111111111".to_owned();
    let cases = [
        ("plenora.message.id", Some(upper.as_str())),
        ("plenora.message.id", None),
        (
            "plenora.trace.correlation_id",
            Some("{22222222-2222-4222-8222-222222222222}"),
        ),
        ("plenora.trace.correlation_id", None),
        ("plenora.message.causation_id", Some("not-a-uuid")),
    ];
    for (key, value) in cases {
        let mut request = with_credential(runtime_request("http://127.0.0.1:9/"));
        match value {
            Some(value) => request.metadata.insert(key.to_owned(), value.to_owned()),
            None => request.metadata.remove(key),
        };
        let label = format!("{key}={value:?}");
        let response = refused(request.clone()).await;
        assert_refusal(&response, "protocol", &label);
        // The result has its own identity; nothing non canonical is copied.
        let id = response.metadata["plenora.message.id"].clone();
        assert_ne!(
            Some(&id),
            request.metadata.get("plenora.message.id"),
            "{label}"
        );
        assert_eq!(
            uuid::Uuid::parse_str(&id).unwrap().hyphenated().to_string(),
            id
        );
        let causation = response.metadata.get("plenora.message.causation_id");
        let correlation = response.metadata.get("plenora.trace.correlation_id");
        if key == "plenora.message.id" {
            assert_eq!(causation, None, "{label}");
        } else {
            assert_eq!(causation.map(String::as_str), Some(MESSAGE_ID), "{label}");
        }
        if key == "plenora.trace.correlation_id" {
            assert_eq!(correlation, None, "{label}");
        } else {
            assert_eq!(
                correlation.map(String::as_str),
                Some(CORRELATION_ID),
                "{label}"
            );
        }
    }
}

#[tokio::test]
async fn deadline_refusals_follow_the_shared_matrix() {
    let deadline = "plenora.execution.deadline";
    // RT-021: not UTC (an offset, `-00:00`) or not RFC 3339 at all.
    for malformed in [
        "2099-01-01T02:00:00+02:00",
        "2099-01-01T00:00:00-00:00",
        "2099-01-01 00:00:00",
        "tomorrow",
    ] {
        let mut request = with_credential(runtime_request("http://127.0.0.1:9/"));
        request
            .metadata
            .insert(deadline.to_owned(), malformed.to_owned());
        assert_refusal(&refused(request).await, "protocol", malformed);
    }

    // Already expired in the metadata: timeout before any resource.
    let mut in_metadata = with_credential(runtime_request("http://127.0.0.1:9/"));
    in_metadata
        .metadata
        .insert(deadline.to_owned(), "2000-01-01T00:00:00Z".to_owned());
    let response = refused(in_metadata).await;
    assert_refusal(&response, "timeout", "metadata");
    assert_eq!(response.payload["code"], "DEADLINE_EXPIRED");

    // RT-023: in the payload the deadline is refused, alone (expired or
    // not) or next to the metadata one, even with the same value.
    let mut expired = with_credential(runtime_request("http://127.0.0.1:9/"));
    expired.payload["options"] = json!({"deadline": "2000-01-01T00:00:00Z"});
    let mut future = with_credential(runtime_request("http://127.0.0.1:9/"));
    future.payload["options"] = json!({"deadline": "2099-01-01T00:00:00Z"});
    let mut both = with_credential(runtime_request("http://127.0.0.1:9/"));
    both.metadata
        .insert(deadline.to_owned(), "2099-01-01T00:00:00Z".to_owned());
    both.payload["options"] = json!({"deadline": "2099-01-01T00:00:00Z"});
    for (label, request) in [("expired", expired), ("future", future), ("both", both)] {
        let response = refused(request).await;
        assert_refusal(&response, "invalid_configuration", label);
        assert_eq!(
            response.payload["code"], "RUNTIME_DEADLINE_IN_PAYLOAD",
            "{label}"
        );
    }
}

/// RT-023: on the runtime the deadline travels only as metadata, so a
/// deadline only in the payload is refused before invocation for every
/// operation, with both input contracts (they declare `options.deadline` for
/// the Rust, CLI and Python surfaces, which have no metadata).
#[tokio::test]
async fn a_deadline_only_in_the_payload_is_refused_for_every_operation() {
    for (operation, contract, payload) in [
        ("rest.test", EXECUTION_REQUEST_CONTRACT, None),
        ("rest.generate", EXECUTION_REQUEST_CONTRACT, None),
        ("rest.enrich", EXECUTION_REQUEST_CONTRACT, None),
        (
            "rest.download",
            FILE_TRANSFER_INPUT_CONTRACT,
            Some(json!({
                "schema_version": 1,
                "operation": "download",
                "connection": {"url": "http://127.0.0.1:9/", "method": "GET"},
                "input": {"file": {"artifact_sink": {"reference": "artifact://tenant/export"}}}
            })),
        ),
        (
            "rest.upload",
            FILE_TRANSFER_INPUT_CONTRACT,
            Some(json!({
                "schema_version": 1,
                "operation": "upload",
                "connection": {"url": "http://127.0.0.1:9/", "method": "PUT"},
                "input": {"file": {"artifact_source": {"reference": "artifact://tenant/import"}}}
            })),
        ),
    ] {
        let mut request = runtime_request("http://127.0.0.1:9/");
        request.metadata.insert(
            "plenora.capability.operation".to_owned(),
            operation.to_owned(),
        );
        request
            .metadata
            .insert("plenora.input.contract".to_owned(), contract.to_owned());
        match payload {
            Some(payload) => request.payload = payload,
            None => {
                request.payload["operation"] = json!(operation.trim_start_matches("rest."));
            }
        }
        request.payload["options"] = json!({"deadline": "2099-01-01T00:00:00Z"});
        let response = refused(request).await;
        assert_refusal(&response, "invalid_configuration", operation);
        assert_eq!(
            response.payload["code"], "RUNTIME_DEADLINE_IN_PAYLOAD",
            "{operation}"
        );
    }
}

#[tokio::test]
async fn idempotency_key_refusals_follow_the_shared_matrix() {
    let key = "plenora.execution.idempotency_key";
    for (label, value) in [("empty", ""), ("space", "a b"), ("long", &"k".repeat(256))] {
        let mut request = with_credential(runtime_request("http://127.0.0.1:9/"));
        request.metadata.insert(key.to_owned(), value.to_owned());
        assert_refusal(&refused(request).await, "protocol", label);
    }

    // A JSON null is neither absent nor a key: protocol, through the
    // serialized transport where it can occur.
    let engine = local_engine();
    let resources = UntouchedResources(std::sync::Mutex::new(0));
    let binding = RuntimeBinding::new(&engine, &resources);
    let mut envelope =
        serde_json::to_value(with_credential(runtime_request("http://127.0.0.1:9/"))).unwrap();
    envelope["metadata"][key] = Value::Null;
    let response: RuntimeMessage = serde_json::from_str(
        &binding
            .invoke_json(&envelope.to_string(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(*resources.0.lock().unwrap(), 0);
    assert_refusal(&response, "protocol", "null");
    assert_eq!(
        response.metadata["plenora.trace.correlation_id"],
        CORRELATION_ID
    );
}

#[tokio::test]
async fn a_success_has_a_new_identity_caused_by_the_request() {
    let (url, server) = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            let body = br#"{"ok":true}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        (format!("http://{address}/"), server)
    };
    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let response =
        invoke_serialized(&binding, runtime_request(&url), CancellationToken::new()).await;
    server.await.unwrap();
    assert_eq!(response.kind, RuntimeMessageKind::Success);
    assert_ne!(response.metadata["plenora.message.id"], MESSAGE_ID);
    assert_eq!(
        response.metadata["plenora.message.causation_id"],
        MESSAGE_ID
    );
    assert_eq!(
        response.metadata["plenora.trace.correlation_id"],
        CORRELATION_ID
    );
    assert_eq!(response.metadata["plenora.operation.version"], "1");
    assert_eq!(
        response.metadata["plenora.capability.operation"],
        "rest.test"
    );
}

#[tokio::test]
async fn unknown_plenora_keys_are_ignored_not_refused() {
    // Keys the binding does not reserve are optional members: a request that
    // carries them is processed as if they were absent (Runtime Binding 1.0
    // §9). A control the operation does not support would be refused under
    // RT-006, but these names are not controls of the binding.
    let mut request = with_credential(runtime_request("http://127.0.0.1:9/"));
    for (key, value) in [
        ("plenora.correlation_id", CORRELATION_ID),
        ("plenora.deadline", "2000-01-01T00:00:00Z"),
        ("plenora.vendor.extension", "x"),
    ] {
        request.metadata.insert(key.to_owned(), value.to_owned());
    }
    let engine = local_engine();
    let resources = UntouchedResources(std::sync::Mutex::new(0));
    let binding = RuntimeBinding::new(&engine, &resources);
    let response = invoke_serialized(&binding, request, CancellationToken::new()).await;
    // Admitted: the credential was resolved and the request was attempted
    // (port 9 refuses), so the failure is a transport one, not a refusal.
    assert_eq!(*resources.0.lock().unwrap(), 1);
    assert_ne!(
        response.payload["phase"], "validate",
        "{:?}",
        response.payload
    );
}

#[tokio::test]
async fn a_malformed_media_type_is_protocol_and_an_unadvertised_one_unsupported() {
    for (content_type, category) in [
        ("application//json", "protocol"),
        ("application/", "protocol"),
        ("/json", "protocol"),
        ("application/json;", "protocol"),
        ("application/json; charset", "protocol"),
        ("application json", "protocol"),
        ("text/csv", "unsupported"),
        ("application/json; charset=utf-8", "unsupported"),
    ] {
        let mut request = with_credential(runtime_request("http://127.0.0.1:9/"));
        request.content_type = content_type.to_owned();
        assert_refusal(&refused(request).await, category, content_type);
    }
}

#[tokio::test]
async fn a_document_that_is_not_an_envelope_is_not_answered_even_with_non_string_metadata() {
    // Found by the runtime_message fuzz target: no payload, and a metadata
    // value that is a number. Without the payload it is not an envelope, so
    // there is nothing to answer, whatever its metadata hold.
    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let text = r#"{"content_type":"application/json","contract":"plenora-runtime-binding-v1","kind":"request","metadata":{"plenora.message.id":"11111111-1111-4111-8111-111111111111","plenora.capability.version":1},"schema_version":1}"#;
    let error = binding
        .invoke_json(text, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.payload().code, "INVALID_INPUT");
}

#[tokio::test]
async fn an_envelope_with_a_duplicate_member_is_not_answered() {
    // Found by the runtime_message fuzz target: two `payload` members. A
    // generic JSON reader keeps the last one in silence; the envelope is
    // refused as malformed, as the typed reader always did.
    let engine = local_engine();
    let resources = EmptyResources;
    let binding = RuntimeBinding::new(&engine, &resources);
    let mut envelope = serde_json::to_value(runtime_request("http://127.0.0.1:9/")).unwrap();
    envelope["metadata"]["plenora.capability.version"] = json!(1);
    let text = envelope.to_string();
    let duplicated = text.replacen("\"payload\":", "\"payload\":{},\"payload\":", 1);
    assert!(
        binding
            .invoke_json(&duplicated, CancellationToken::new())
            .await
            .is_err()
    );
    // The same envelope without the duplicate is answered with a protocol
    // refusal for its numeric metadata value.
    let answered: RuntimeMessage = serde_json::from_str(
        &binding
            .invoke_json(&text, CancellationToken::new())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(answered.payload["category"], "protocol");
}
