//! Runtime Vectors 1.0 (`plenora-runtime-vector-v1`): the REST fixtures of the
//! adopted `plenora-contracts` revision, exercised through the public runtime
//! binding.
//!
//! The fixtures are copied byte for byte into `contracts/upstream` and pinned
//! by SHA-256 in `contracts/upstream/source.json`, together with the revision
//! they come from; the first test refuses a drifted copy. The specification
//! (§4) asks an adopter to exercise every fixture whose operation it
//! advertises, and to show fail-closed rejection when the routing identity of
//! a request fixture is missing or invalid. REST advertises `rest.upload` and
//! `rest.download`, the operations of the three REST fixtures.
//!
//! Fixture payloads are illustrative (§5): they show the runtime shape, not
//! the component-owned `plenora-rest-file-transfer-*` schemas. The request
//! fixture is therefore sent twice: verbatim, where the binding must refuse
//! the payload with a typed error after accepting the routing; and with the
//! same metadata and the equivalent component payload (the same credential
//! and artifact references, the same byte count), where it must succeed. The
//! success and error fixtures are compared field by field with what the
//! binding returns for the equivalent operation against a local server.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use plenora_rest_core::{
    AuthConfig, CancellationToken, ERROR_CONTENT_TYPE, ERROR_CONTRACT, Engine, EngineConfig,
    EngineError, FILE_TRANSFER_INPUT_CONTRACT, FILE_TRANSFER_RESULT_CONTRACT, RuntimeBinding,
    RuntimeMessage, RuntimeMessageKind, RuntimeResources, capabilities,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

const UPLOAD_REQUEST: &str = "runtime-v1/rest-upload-request.json";
const DOWNLOAD_SUCCESS: &str = "runtime-v1/rest-download-success.json";
const UPLOAD_UNKNOWN_ERROR: &str = "runtime-v1/rest-upload-unknown-error.json";

/// Metadata keys that route a request; each one is mutated by the negative
/// probes of §4.
const ROUTING_KEYS: [&str; 5] = [
    "plenora.capability.name",
    "plenora.capability.version",
    "plenora.capability.operation",
    "plenora.operation.version",
    "plenora.input.contract",
];

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn upstream_root() -> PathBuf {
    repository_root().join("contracts").join("upstream")
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn vector(name: &str) -> Value {
    read_json(&upstream_root().join(name))
}

/// The runtime message a transport builds from a fixture: its envelope,
/// content type and metadata, with `payload` in place of the fixture payload.
/// The fixture's `$schema` member is editorial and not part of the envelope.
fn message(vector: &Value, payload: Value) -> RuntimeMessage {
    let metadata = vector["metadata"]
        .as_object()
        .expect("fixture metadata is an object")
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value.as_str().expect("fixture metadata is text").to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    serde_json::from_value(json!({
        "schema_version": vector["schema_version"],
        "contract": vector["contract"],
        "kind": vector["kind"],
        "content_type": vector["content_type"],
        "metadata": metadata,
        "payload": payload,
    }))
    .expect("fixture envelope is a runtime message")
}

/// Request metadata for a result fixture, which carries only the response
/// half of the routing identity: same correlation, operation and version.
fn request_metadata_for(result: &Value, input_contract: &str) -> Value {
    let metadata = &result["metadata"];
    json!({
        "schema_version": 1,
        "contract": "plenora-runtime-vector-v1",
        "kind": "request",
        "content_type": "application/json",
        "metadata": {
            "plenora.message.id": "018f3d84-7b2c-7f00-8000-0000000001ff",
            "plenora.capability.name": "plenora.rest-tools",
            "plenora.capability.version": "1",
            "plenora.capability.operation": metadata["plenora.capability.operation"],
            "plenora.operation.version": metadata["plenora.operation.version"],
            "plenora.input.contract": input_contract,
            "plenora.trace.correlation_id": metadata["plenora.trace.correlation_id"],
        },
        "payload": {},
    })
}

/// Resources that answer only the references the fixtures name, and record
/// every call so a probe can prove nothing was resolved.
struct VectorResources {
    directory: PathBuf,
    credentials: Vec<String>,
    sources: BTreeMap<String, PathBuf>,
    sinks: BTreeMap<String, PathBuf>,
    calls: Mutex<Vec<String>>,
}

impl VectorResources {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "plenora-rest-vectors-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        Self {
            directory,
            credentials: Vec::new(),
            sources: BTreeMap::new(),
            sinks: BTreeMap::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn engine(&self) -> Engine {
        Engine::new(EngineConfig {
            allow_private_networks: true,
            allow_file_transfers: true,
            file_root: Some(self.directory.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Drop for VectorResources {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn unknown_reference() -> EngineError {
    EngineError::InvalidInput("reference is not authorized by the host".into())
}

impl RuntimeResources for VectorResources {
    fn resolve_credentials(&self, reference: &str) -> Result<AuthConfig, EngineError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("credential {reference}"));
        if self.credentials.iter().any(|known| known == reference) {
            Ok(AuthConfig::None)
        } else {
            Err(unknown_reference())
        }
    }

    fn resolve_artifact_source(&self, reference: &str) -> Result<PathBuf, EngineError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("source {reference}"));
        self.sources
            .get(reference)
            .cloned()
            .ok_or_else(unknown_reference)
    }

    fn resolve_artifact_sink(&self, reference: &str) -> Result<PathBuf, EngineError> {
        self.calls.lock().unwrap().push(format!("sink {reference}"));
        self.sinks
            .get(reference)
            .cloned()
            .ok_or_else(unknown_reference)
    }
}

/// Reads one HTTP/1.1 request, head and body (fixed length or chunked).
async fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "connection closed before the request head");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let header = |name: &str| {
        head.lines().find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim().to_owned())
        })
    };
    let mut raw = buffer[header_end..].to_vec();
    if header("transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked")) {
        while !raw.windows(5).any(|window| window == b"0\r\n\r\n") {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection closed inside a chunked body");
            raw.extend_from_slice(&chunk[..read]);
        }
        let mut body = Vec::new();
        let mut rest = raw.as_slice();
        loop {
            let line_end = rest
                .windows(2)
                .position(|window| window == b"\r\n")
                .unwrap();
            let size_text = String::from_utf8_lossy(&rest[..line_end]);
            let size =
                usize::from_str_radix(size_text.split(';').next().unwrap().trim(), 16).unwrap();
            rest = &rest[line_end + 2..];
            if size == 0 {
                break;
            }
            body.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
        (head, body)
    } else {
        let length = header("content-length").map_or(0, |value| value.parse().unwrap());
        while raw.len() < length {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection closed inside the body");
            raw.extend_from_slice(&chunk[..read]);
        }
        (head, raw)
    }
}

/// One-request server: answers with `status`, `content_type` and `body`.
async fn serving(
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
) -> (String, JoinHandle<(String, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/vector", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        stream.shutdown().await.unwrap();
        request
    });
    (url, task)
}

/// One-request server that takes the whole request and never answers: the
/// caller cannot know whether the remote side acted on it.
async fn silent() -> (String, JoinHandle<(String, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/vector", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        // Hold the connection until the client gives up on it.
        let mut sink = [0_u8; 64];
        while stream.read(&mut sink).await.unwrap_or(0) > 0 {}
        request
    });
    (url, task)
}

async fn invoke(resources: &VectorResources, request: RuntimeMessage) -> RuntimeMessage {
    let engine = resources.engine();
    let binding = RuntimeBinding::new(&engine, resources);
    let request_json = serde_json::to_string(&request).unwrap();
    let response = binding
        .invoke_json(&request_json, CancellationToken::new())
        .await
        .expect("the binding always answers with an envelope");
    serde_json::from_str(&response).unwrap()
}

/// The common response identity: the correlation and operation of the
/// request, a fresh message identifier, causation set to the request.
fn assert_response_identity(response: &RuntimeMessage, request: &RuntimeMessage) {
    for key in [
        "plenora.trace.correlation_id",
        "plenora.capability.operation",
        "plenora.operation.version",
    ] {
        assert_eq!(
            response.metadata.get(key),
            request.metadata.get(key),
            "{key}"
        );
    }
    assert_eq!(
        response.metadata.get("plenora.message.causation_id"),
        request.metadata.get("plenora.message.id")
    );
    assert_ne!(
        response.metadata.get("plenora.message.id"),
        request.metadata.get("plenora.message.id")
    );
}

#[test]
fn vendored_contract_files_match_their_pins() {
    let source = read_json(&upstream_root().join("source.json"));
    let manifest = read_json(&repository_root().join("adoption-manifest.json"));
    assert_eq!(
        source["revision"], manifest["contracts_source"]["revision"],
        "contracts/upstream must come from the revision the manifest adopts"
    );
    assert_eq!(
        source["repository"],
        manifest["contracts_source"]["repository"]
    );
    let pins = source["files"].as_object().expect("pinned files");
    for (name, pin) in pins {
        let bytes = fs::read(upstream_root().join(name)).unwrap();
        assert_eq!(
            sha256_hex(&bytes),
            pin["sha256"].as_str().unwrap(),
            "{name} differs from the pinned upstream copy"
        );
    }
    for name in [UPLOAD_REQUEST, DOWNLOAD_SUCCESS, UPLOAD_UNKNOWN_ERROR] {
        assert!(pins.contains_key(name), "{name} is not pinned");
    }
    // The complete probe set of the adopted revision (Runtime Vectors 1.0
    // §6), exercised by `rejection_probes_hold_on_the_rest_request_vector`.
    assert!(pins.contains_key("schemas/runtime-probe-v1.schema.json"));
    assert_eq!(
        pins.keys()
            .filter(|name| name.starts_with("runtime-probes-v1/"))
            .count(),
        21,
        "every rejection probe of the adopted revision is vendored"
    );
    // Every REST fixture is exercised below, and only REST fixtures are
    // vendored: one with another operation would be dead weight.
    let advertised = capabilities()
        .operations
        .iter()
        .map(|operation| operation.id.clone())
        .collect::<Vec<_>>();
    for name in pins.keys().filter(|name| name.starts_with("runtime-v1/")) {
        let operation = vector(name)["metadata"]["plenora.capability.operation"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(advertised.contains(&operation), "{name}: {operation}");
        assert!(
            [UPLOAD_REQUEST, DOWNLOAD_SUCCESS, UPLOAD_UNKNOWN_ERROR].contains(&name.as_str()),
            "{name} is vendored but not exercised"
        );
    }
}

#[tokio::test]
async fn upload_request_fixture_is_routed_and_its_illustrative_payload_refused() {
    let fixture = vector(UPLOAD_REQUEST);
    let resources = VectorResources::new("verbatim");
    let request = message(&fixture, fixture["payload"].clone());

    let response = invoke(&resources, request.clone()).await;

    // Routing is accepted; the illustrative payload is not a
    // plenora-rest-file-transfer-input-v1 document, so it is refused as typed
    // invalid input before any resource is resolved or network is touched.
    assert_eq!(response.kind, RuntimeMessageKind::Error);
    assert_eq!(response.content_type, ERROR_CONTENT_TYPE);
    assert_eq!(response.metadata["plenora.output.contract"], ERROR_CONTRACT);
    assert_response_identity(&response, &request);
    assert_eq!(response.payload["category"], "invalid_configuration");
    assert_eq!(response.payload["code"], "INVALID_INPUT");
    assert_eq!(response.payload["phase"], "validate");
    assert_eq!(response.payload["remote_effect"], "none");
    assert_eq!(response.payload["retry"]["kind"], "never");
    assert!(resources.calls().is_empty(), "{:?}", resources.calls());
}

/// The upload fixture's payload in the component-owned input contract: the
/// same credential and artifact references, the same byte bound.
fn component_upload_payload(fixture: &Value, url: &str, sha256: &str) -> Value {
    let payload = &fixture["payload"];
    json!({
        "schema_version": 1,
        "operation": "upload",
        "connection": {
            "url": url,
            "method": "PUT",
            "credential_ref": payload["connection_ref"],
            "request": {"body_type": "raw"},
            "response": {"format": "text"},
            "retry": {"max_attempts": 1}
        },
        "input": {
            "file": {
                "artifact_source": {"reference": payload["artifact_source"]["ref"]},
                "content_type": payload["artifact_source"]["content_type"],
                "expected_sha256": sha256
            }
        }
    })
}

fn upload_resources(name: &str, fixture: &Value, content: &[u8]) -> VectorResources {
    let payload = &fixture["payload"];
    let mut resources = VectorResources::new(name);
    let path = resources.directory.join("source.bin");
    fs::write(&path, content).unwrap();
    resources
        .credentials
        .push(payload["connection_ref"].as_str().unwrap().to_owned());
    resources.sources.insert(
        payload["artifact_source"]["ref"]
            .as_str()
            .unwrap()
            .to_owned(),
        path,
    );
    resources
}

#[tokio::test]
async fn upload_request_fixture_succeeds_with_the_equivalent_component_payload() {
    let fixture = vector(UPLOAD_REQUEST);
    let byte_count = usize::try_from(
        fixture["payload"]["artifact_source"]["byte_count"]
            .as_u64()
            .unwrap(),
    )
    .unwrap();
    let content = vec![b'v'; byte_count];
    let resources = upload_resources("upload", &fixture, &content);
    let (url, server) = serving("200 OK", "text/plain", b"stored".to_vec()).await;
    let request = message(
        &fixture,
        component_upload_payload(&fixture, &url, &sha256_hex(&content)),
    );

    let response = invoke(&resources, request.clone()).await;
    let (head, body) = server.await.unwrap();

    assert_eq!(
        response.kind,
        RuntimeMessageKind::Success,
        "{:?}",
        response.payload
    );
    assert_eq!(response.content_type, "application/json");
    assert_eq!(
        response.metadata["plenora.output.contract"],
        FILE_TRANSFER_RESULT_CONTRACT
    );
    assert_response_identity(&response, &request);
    assert!(head.starts_with("PUT /vector "), "{head}");
    assert_eq!(body, content);
    let output = &response.payload["output"];
    assert_eq!(output["direction"], "upload");
    assert_eq!(
        output["artifact_reference"],
        fixture["payload"]["artifact_source"]["ref"]
    );
    assert_eq!(output["bytes_transferred"], byte_count);
    assert_eq!(output["checksum"]["algorithm"], "sha256");
    assert_eq!(output["checksum"]["value"], sha256_hex(&content));
    assert_eq!(
        resources.calls(),
        [
            format!(
                "credential {}",
                fixture["payload"]["connection_ref"].as_str().unwrap()
            ),
            format!(
                "source {}",
                fixture["payload"]["artifact_source"]["ref"]
                    .as_str()
                    .unwrap()
            ),
        ]
    );
}

#[tokio::test]
async fn upload_request_fixture_fails_closed_on_every_routing_mutation() {
    let fixture = vector(UPLOAD_REQUEST);
    let content = vec![b'v'; 17];
    // No server listens on port 9; reaching it would fail differently.
    let payload = component_upload_payload(&fixture, "http://127.0.0.1:9/", &sha256_hex(&content));
    let valid = message(&fixture, payload);
    let wrong_values = [
        ("plenora.capability.name", "plenora.storage-tools"),
        ("plenora.capability.version", "2"),
        ("plenora.capability.operation", "rest.delete"),
        ("plenora.operation.version", "2"),
        (
            "plenora.input.contract",
            "plenora-rest-execution-request-v1",
        ),
    ];
    let mut probes = Vec::new();
    for key in ROUTING_KEYS {
        let mut missing = valid.clone();
        missing.metadata.remove(key);
        probes.push((format!("missing {key}"), missing));
    }
    for (key, value) in wrong_values {
        let mut wrong = valid.clone();
        wrong.metadata.insert(key.to_owned(), value.to_owned());
        probes.push((format!("{key}={value}"), wrong));
    }
    // An operation that disagrees with the input contract of the payload.
    let mut swapped = valid.clone();
    swapped.metadata.insert(
        "plenora.capability.operation".to_owned(),
        "rest.download".to_owned(),
    );
    probes.push(("upload payload routed as download".to_owned(), swapped));

    for (label, probe) in probes {
        let resources = upload_resources("routing", &fixture, &content);
        let response = invoke(&resources, probe.clone()).await;
        assert_eq!(response.kind, RuntimeMessageKind::Error, "{label}");
        assert_eq!(response.content_type, ERROR_CONTENT_TYPE, "{label}");
        assert_eq!(
            response.metadata["plenora.output.contract"], ERROR_CONTRACT,
            "{label}"
        );
        assert_eq!(response.payload["phase"], "validate", "{label}");
        assert_eq!(response.payload["remote_effect"], "none", "{label}");
        assert_eq!(response.payload["retry"]["kind"], "never", "{label}");
        // R1 of the shared runtime matrix: absent is `protocol`, a
        // well-formed value this component does not advertise is
        // `unsupported`; a payload that does not fit the routed operation is
        // a payload error.
        let expected = if label.starts_with("missing") {
            "protocol"
        } else if label.starts_with("upload payload") {
            "invalid_configuration"
        } else {
            "unsupported"
        };
        assert_eq!(
            response.payload["category"], expected,
            "{label}: {:?}",
            response.payload
        );
        assert!(
            resources.calls().is_empty(),
            "{label}: {:?}",
            resources.calls()
        );
        assert_eq!(
            response.metadata.get("plenora.trace.correlation_id"),
            probe.metadata.get("plenora.trace.correlation_id"),
            "{label}"
        );
    }
}

#[tokio::test]
async fn download_success_fixture_is_the_result_of_the_equivalent_download() {
    let fixture = vector(DOWNLOAD_SUCCESS);
    let artifact = &fixture["payload"]["artifact"];
    let byte_count = usize::try_from(artifact["byte_count"].as_u64().unwrap()).unwrap();
    let content = vec![b'd'; byte_count];
    let sink_reference = artifact["ref"].as_str().unwrap().to_owned();
    let mut resources = VectorResources::new("download");
    let sink = resources.directory.join("sink.bin");
    resources.sinks.insert(sink_reference.clone(), sink.clone());
    let (url, server) = serving("200 OK", "application/octet-stream", content.clone()).await;
    let envelope = request_metadata_for(&fixture, FILE_TRANSFER_INPUT_CONTRACT);
    let request = message(
        &envelope,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": url, "method": "GET", "retry": {"max_attempts": 1}},
            "input": {"file": {"artifact_sink": {"reference": sink_reference}}}
        }),
    );

    let response = invoke(&resources, request.clone()).await;
    let (head, _) = server.await.unwrap();

    assert!(head.starts_with("GET /vector "), "{head}");
    assert_eq!(
        response.kind,
        RuntimeMessageKind::Success,
        "{:?}",
        response.payload
    );
    // Envelope: everything the fixture states about it.
    assert_eq!(response.content_type, fixture["content_type"]);
    for key in [
        "plenora.capability.operation",
        "plenora.operation.version",
        "plenora.output.contract",
        "plenora.trace.correlation_id",
    ] {
        assert_eq!(
            response.metadata[key], fixture["metadata"][key],
            "{key} differs from the fixture"
        );
    }
    assert_response_identity(&response, &request);
    // Payload: the fixture's artifact, in the component-owned result shape.
    // The fixture's checksum value is illustrative; the result carries the
    // checksum actually computed over the bytes written.
    let output = &response.payload["output"];
    assert_eq!(output["direction"], "download");
    assert_eq!(output["artifact_reference"], artifact["ref"]);
    assert_eq!(output["bytes_transferred"], artifact["byte_count"]);
    assert_eq!(output["media_type"], artifact["content_type"]);
    assert_eq!(
        output["checksum"]["algorithm"],
        artifact["checksum"]["algorithm"]
    );
    assert_eq!(output["checksum"]["value"], sha256_hex(&content));
    assert_eq!(fs::read(&sink).unwrap(), content);
    assert!(
        !serde_json::to_string(&response)
            .unwrap()
            .contains(&*sink.to_string_lossy()),
        "the resolved sink path must not cross the boundary"
    );
}

#[tokio::test]
async fn upload_unknown_error_fixture_is_the_failure_of_an_unconfirmed_upload() {
    let fixture = vector(UPLOAD_UNKNOWN_ERROR);
    let request_fixture = vector(UPLOAD_REQUEST);
    let content = vec![b'u'; 17];
    let resources = upload_resources("unknown", &request_fixture, &content);
    let (url, server) = silent().await;
    let mut payload = component_upload_payload(&request_fixture, &url, &sha256_hex(&content));
    payload["connection"]["request"]["timeout_ms"] = json!(300);
    let mut envelope = request_metadata_for(&fixture, FILE_TRANSFER_INPUT_CONTRACT);
    envelope["metadata"]["plenora.message.id"] =
        request_fixture["metadata"]["plenora.message.id"].clone();
    let request = message(&envelope, payload);

    let response = invoke(&resources, request.clone()).await;
    let (_, body) = server.await.unwrap();

    // The body reached the remote side and no answer came back.
    assert_eq!(body, content);
    assert_eq!(
        response.kind,
        RuntimeMessageKind::Error,
        "{:?}",
        response.payload
    );
    assert_eq!(response.content_type, fixture["content_type"]);
    for key in [
        "plenora.capability.operation",
        "plenora.operation.version",
        "plenora.output.contract",
        "plenora.trace.correlation_id",
    ] {
        assert_eq!(
            response.metadata[key], fixture["metadata"][key],
            "{key} differs from the fixture"
        );
    }
    assert_response_identity(&response, &request);
    // The axes the fixture fixes: what failed and what is known remotely.
    let error = &response.payload;
    assert_eq!(error["category"], fixture["payload"]["category"]);
    assert_eq!(error["remote_effect"], fixture["payload"]["remote_effect"]);
    // ERR-006: an unknown remote effect never allows an automatic retry. The
    // fixture shows `requires_recovery`; the engine has no recovery handle
    // for a plain upload and quarantines, the other admitted disposition.
    assert!(
        ["never", "quarantine", "requires_recovery"]
            .contains(&error["retry"]["kind"].as_str().unwrap()),
        "{error}"
    );
    assert!(error.get("retry").unwrap().get("delay_ms").is_none());
    // Code and message are component-owned; the message never quotes the
    // remote side or the resolved path.
    assert!(error["code"].as_str().is_some_and(|code| !code.is_empty()));
    let text = serde_json::to_string(&response).unwrap();
    assert!(!text.contains(&*resources.directory.to_string_lossy()));
}

/// The REST negative examples of the adopted revision
/// (`examples/invalid/rest-*.json`), restated as runtime requests. The
/// contracts gate judges those documents heuristically; the property itself
/// is shown here, at the boundary that enforces it.
#[tokio::test]
async fn contract_negative_examples_are_refused_at_the_runtime_boundary() {
    const SECRET: &str = "Bearer do-not-persist";
    let fixture = vector(UPLOAD_REQUEST);
    let routed = |operation: &str, payload: Value| {
        let mut request = message(&fixture, payload);
        request.metadata.insert(
            "plenora.capability.operation".to_owned(),
            format!("rest.{operation}"),
        );
        request
    };
    let download = |file: Value| {
        routed(
            "download",
            json!({
                "schema_version": 1,
                "operation": "download",
                "connection": {"url": "http://127.0.0.1:9/", "method": "GET",
                               "credential_ref": "secret://rest/production"},
                "input": {"file": file}
            }),
        )
    };
    let upload = |connection: Value, file: Value| {
        let mut base = json!({"url": "http://127.0.0.1:9/", "method": "PUT"});
        for (key, value) in connection.as_object().unwrap() {
            base[key] = value.clone();
        }
        routed(
            "upload",
            json!({
                "schema_version": 1,
                "operation": "upload",
                "connection": base,
                "input": {"file": file}
            }),
        )
    };
    let sink = |reference: &str| json!({"artifact_sink": {"reference": reference}});
    let source = json!({"artifact_source": {"reference": "artifact://tenant-a/8d936f1d"}});
    let cases = [
        // rest-runtime-artifact-local-path.json
        (
            "local path sink",
            download(sink(r"C:\private\exports\report.csv")),
        ),
        // rest-runtime-artifact-relative-path.json, and the relative spellings
        // the profile's conformance note says the gate heuristic lets through.
        ("relative path sink", download(sink("../private/file"))),
        ("bare relative sink", download(sink("dir/report.csv"))),
        ("bare file name sink", download(sink("report.csv"))),
        (
            "file URL sink",
            download(sink("file:///private/report.csv")),
        ),
        // rest-download-artifact-source-only.json
        ("download with a source", download(source.clone())),
        // rest-upload-artifact-sink-only.json
        (
            "upload with a sink",
            upload(json!({}), sink("artifact://tenant-a/8d936f1d")),
        ),
        // rest-runtime-upload-inline-credentials.json, as written and in the
        // spellings the component contract would otherwise carry.
        (
            "authorization member",
            upload(json!({"authorization": SECRET}), source.clone()),
        ),
        (
            "authorization header",
            upload(
                json!({"headers": {"Authorization": SECRET}}),
                source.clone(),
            ),
        ),
        (
            "inline bearer auth",
            upload(
                json!({"auth": {"type": "bearer", "token": SECRET}}),
                source.clone(),
            ),
        ),
    ];
    for (label, request) in cases {
        // The credential resolves, so a refusal can only come from the
        // artifact or credential rules under test, and an artifact that got
        // as far as the host shows up in `calls`.
        let mut resources = VectorResources::new("negative");
        resources
            .credentials
            .push("secret://rest/production".to_owned());
        let response = invoke(&resources, request.clone()).await;
        assert_eq!(response.kind, RuntimeMessageKind::Error, "{label}");
        assert_eq!(
            response.payload["code"], "INVALID_INPUT",
            "{label}: {:?}",
            response.payload
        );
        assert_eq!(response.payload["remote_effect"], "none", "{label}");
        assert!(
            resources
                .calls()
                .iter()
                .all(|call| !call.starts_with("source") && !call.starts_with("sink")),
            "{label}: an artifact was resolved: {:?}",
            resources.calls()
        );
        assert!(
            !serde_json::to_string(&response)
                .unwrap()
                .contains("do-not-persist"),
            "{label}: the secret crossed back"
        );
        assert_response_identity(&response, &request);
    }

    // rest-download-local-mutating-method.json: a download may use any method
    // the caller configures, so its declared side effect is never `local`.
    let document = serde_json::to_value(capabilities()).unwrap();
    for operation in document["operations"].as_array().unwrap() {
        assert_eq!(operation["side_effect"], "remote", "{}", operation["id"]);
    }
}

/// Rejection probes of Runtime Binding 1.0 §11-13 (RT-016 to RT-022) and
/// Runtime Vectors 1.0 §6, from the adopted revision (plenora-contracts
/// v1.1.0): copied byte for byte into `contracts/upstream/runtime-probes-v1`
/// and pinned there like the other vendored files.
///
/// Each probe is one metadata mutation of a request vector. The probes are
/// written against several components' vectors; the routing rules they
/// exercise are the same for every component, so each mutation is applied to
/// the REST upload request vector, and the expected metadata is read with
/// that base: a key the probe expects keeps the base value, or the mutated
/// value when the probe mutated that very key. The probes whose base is the
/// REST request vector, which §6 requires of this adopter, run against their
/// own base exactly.
#[tokio::test]
async fn rejection_probes_hold_on_the_rest_request_vector() {
    let source = read_json(&upstream_root().join("source.json"));
    let pins = source["files"].as_object().unwrap();
    let base = vector(UPLOAD_REQUEST);
    let mut exercised = 0;
    let mut own_base = 0;
    for name in pins
        .keys()
        .filter(|name| name.starts_with("runtime-probes-v1/"))
    {
        let probe = read_json(&upstream_root().join(name));
        let expected = &probe["expected"];
        if probe["base"]
            .as_str()
            .is_some_and(|base| base.starts_with("rest-"))
        {
            assert_eq!(probe["base"], "rest-upload-request.json", "{name}");
            own_base += 1;
        }
        // RT-006 for a control REST advertises: idempotency keys are
        // supported by every rest.* operation, so this probe's premise does
        // not exist here.
        if name.ends_with("storage-get-idempotency-key-unsupported.json") {
            continue;
        }
        let mut envelope = json!({
            "schema_version": base["schema_version"],
            "contract": base["contract"],
            "kind": base["kind"],
            "content_type": base["content_type"],
            "metadata": base["metadata"],
            "payload": base["payload"],
        });
        let mutation = &probe["mutation"];
        let mutated_key = if let Some(set) = mutation["set"].as_object() {
            let (key, value) = set.iter().next().unwrap();
            envelope["metadata"][key] = value.clone();
            key.clone()
        } else {
            let key = mutation["remove"].as_str().unwrap().to_owned();
            envelope["metadata"].as_object_mut().unwrap().remove(&key);
            key
        };
        let resources = VectorResources::new("probe");
        let engine = resources.engine();
        let binding = RuntimeBinding::new(&engine, &resources);
        let response: RuntimeMessage = serde_json::from_str(
            &binding
                .invoke_json(&envelope.to_string(), CancellationToken::new())
                .await
                .expect("an envelope is always answered"),
        )
        .unwrap();
        assert!(
            resources.calls().is_empty(),
            "{name}: {:?}",
            resources.calls()
        );
        assert_eq!(response.kind, RuntimeMessageKind::Error, "{name}");
        assert_eq!(response.content_type, expected["content_type"], "{name}");
        for axis in ["category", "phase", "remote_effect", "retry"] {
            assert_eq!(
                response.payload[axis], expected["error"][axis],
                "{name}: {axis} in {:?}",
                response.payload
            );
        }
        let listed = expected["metadata"].as_object().unwrap();
        for key in [
            "plenora.capability.operation",
            "plenora.operation.version",
            "plenora.output.contract",
            "plenora.trace.correlation_id",
        ] {
            let want = listed.get(key).map(|value| {
                if key == "plenora.output.contract" {
                    value.clone()
                } else if key == mutated_key {
                    envelope["metadata"][key].clone()
                } else {
                    base["metadata"][key].clone()
                }
            });
            assert_eq!(
                response.metadata.get(key).map(|value| json!(value)),
                want,
                "{name}: {key}"
            );
        }
        // RT-020: a new identity, caused by the request when its id is
        // canonical, never copying the request's own causation.
        assert_ne!(
            response
                .metadata
                .get("plenora.message.id")
                .map(String::as_str),
            envelope["metadata"]["plenora.message.id"].as_str(),
            "{name}"
        );
        let request_id = envelope["metadata"]["plenora.message.id"].as_str();
        let canonical = request_id.is_some_and(|id| {
            uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
        });
        assert_eq!(
            response
                .metadata
                .get("plenora.message.causation_id")
                .map(String::as_str),
            if canonical { request_id } else { None },
            "{name}"
        );
        exercised += 1;
    }
    assert_eq!(exercised, 20, "every applicable probe is exercised");
    assert_eq!(
        own_base, 2,
        "both probes on the REST request vector are exercised"
    );
}
