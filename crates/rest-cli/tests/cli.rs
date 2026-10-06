//! Test black-box del binario `plenora-rest`: lo lanciano come processo e
//! controllano soltanto ciò che vede un orchestratore, cioè stdout, stderr ed
//! exit code. Ogni documento JSON è validato contro gli schemi comuni
//! (`cli-envelope-v2`, che include `error-v1`, e `capabilities-v2`) copiati
//! da plenora-contracts in `tests/fixtures/contracts`.

mod support;

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::schema::Registry;

const ENVELOPE: &str = "https://schemas.plenora.dev/cli-envelope-v2.schema.json";
const CAPABILITIES: &str = "https://schemas.plenora.dev/capabilities-v2.schema.json";
const OPERATIONS: [&str; 5] = ["test", "generate", "enrich", "download", "upload"];

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_plenora-rest"))
}

fn run(arguments: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut child = binary()
        .args(arguments)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(bytes) = stdin {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    child.wait_with_output().unwrap()
}

/// Un'invocazione in modalità JSON: esattamente un documento e un newline su
/// stdout, niente su stderr, envelope valido, exit 0 se e solo se `ok`.
struct Machine {
    document: Value,
    exit: i32,
}

fn machine_output(output: &Output) -> Machine {
    assert!(
        output.stderr.is_empty(),
        "stderr must stay empty in JSON mode: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = std::str::from_utf8(&output.stdout).expect("stdout must be UTF-8");
    let line = stdout
        .strip_suffix('\n')
        .expect("the document must end with a newline");
    assert!(
        !line.contains('\n'),
        "stdout must hold exactly one JSON document"
    );
    let document: Value = serde_json::from_str(line).expect("stdout must be one JSON document");
    Registry::load()
        .validate(ENVELOPE, &document)
        .unwrap_or_else(|error| panic!("invalid envelope: {error}\n{document}"));
    let exit = output
        .status
        .code()
        .expect("the process must exit normally");
    assert_eq!(
        exit == 0,
        document["status"] == "ok",
        "exit 0 only with status ok: {document}"
    );
    assert_eq!(document["component"], "plenora-rest-tools");
    assert_eq!(document["component_version"], env!("CARGO_PKG_VERSION"));
    Machine { document, exit }
}

fn machine(arguments: &[&str], stdin: Option<&[u8]>) -> Machine {
    machine_output(&run(arguments, stdin))
}

fn assert_error(machine: &Machine, exit: i32, category: &str, code: &str) {
    assert_eq!(machine.document["status"], "error", "{}", machine.document);
    assert_eq!(machine.exit, exit, "{}", machine.document);
    assert_eq!(
        machine.document["error"]["category"], category,
        "{}",
        machine.document
    );
    assert_eq!(
        machine.document["error"]["code"], code,
        "{}",
        machine.document
    );
}

fn temp_dir(label: &str) -> PathBuf {
    static SEQUENCE: AtomicU32 = AtomicU32::new(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "plenora-rest-cli-{label}-{}-{unique}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn write_json(directory: &Path, name: &str, value: &Value) -> String {
    let path = directory.join(name);
    std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    path.to_string_lossy().into_owned()
}

fn private_networks() -> Value {
    json!({"allow_private_networks": true})
}

/// Un server HTTP locale che risponde una volta con `status` e `body`.
fn serve_once(status: u16, body: Vec<u8>) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/items", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_head(&mut stream);
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        stream.flush().unwrap();
        request
    });
    (url, handle)
}

fn read_head(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

// ---------------------------------------------------------------- discovery

#[test]
fn help_is_human_text_on_stdout_with_exit_zero() {
    let output = run(&["--help"], None);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("plenora-rest"));
    for command in ["--version --format json", "capabilities --format json"] {
        assert!(text.contains(command), "{command}");
    }
    for operation in OPERATIONS {
        assert!(text.contains(&format!("  {operation} ")), "{operation}");
    }
}

#[test]
fn version_reports_component_and_protocol() {
    let version = machine(&["--version", "--format", "json"], None);
    assert_eq!(version.exit, 0);
    assert_eq!(version.document["command"], "version");
    assert_eq!(
        version.document["contract"],
        "plenora-rest-version-result-v1"
    );
    assert_eq!(
        version.document["result"],
        json!({"component_version": env!("CARGO_PKG_VERSION"), "protocol_version": 2})
    );
}

#[test]
fn capabilities_validate_and_declare_the_cli() {
    let capabilities = machine(&["capabilities", "--format", "json"], None);
    assert_eq!(capabilities.exit, 0);
    assert_eq!(capabilities.document["command"], "capabilities");
    assert_eq!(capabilities.document["contract"], "plenora-capabilities-v2");
    let result = &capabilities.document["result"];
    Registry::load()
        .validate(CAPABILITIES, result)
        .unwrap_or_else(|error| panic!("invalid capabilities: {error}"));
    let cli: Vec<&Value> = result["interfaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|interface| interface["kind"] == "cli")
        .collect();
    assert_eq!(
        cli,
        [
            &json!({"kind": "cli", "contract": "plenora-cli-v2", "version": 2, "artifact": "plenora-rest"})
        ]
    );
    let operations = result["operations"].as_array().unwrap();
    let ids: Vec<&str> = operations
        .iter()
        .map(|operation| operation["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, OPERATIONS.map(|operation| format!("rest.{operation}")));
    for operation in operations {
        assert!(
            operation["surfaces"]
                .as_array()
                .unwrap()
                .contains(&json!("cli")),
            "{operation}"
        );
    }
    assert_eq!(result["component_version"], env!("CARGO_PKG_VERSION"));
}

// ------------------------------------------------------ fail-closed parsing

#[test]
fn unknown_commands_answer_with_the_error_contract() {
    for command in ["delete", "--verbose", "frobnicate"] {
        let rejected = machine(&[command, "--format", "json"], None);
        assert_error(&rejected, 2, "invalid_configuration", "UNKNOWN_COMMAND");
        assert_eq!(rejected.document["command"], "unknown");
        assert_eq!(rejected.document["contract"], "plenora-error-v1");
        assert!(!rejected.document.to_string().contains(command));
    }
}

#[test]
fn bad_flags_and_arguments_fail_closed_with_exit_two() {
    let cases: [(&[&str], &str); 6] = [
        (
            &["test", "--input", "x.json", "--format", "json", "--verbose"],
            "UNKNOWN_FLAG",
        ),
        (&["test", "--format", "json", "--input"], "MISSING_VALUE"),
        (
            &["test", "--input", "x.json", "extra", "--format", "json"],
            "UNEXPECTED_ARGUMENT",
        ),
        (
            &["test", "--input", "a", "--input", "b", "--format", "json"],
            "DUPLICATE_FLAG",
        ),
        (&["download", "--format", "json"], "MISSING_INPUT"),
        (
            &["capabilities", "--format", "json", "now"],
            "UNEXPECTED_ARGUMENT",
        ),
    ];
    // `--help` è solo testo umano: chiesto in JSON, è un flag rifiutato.
    let help = machine(&["--help", "--format", "json"], None);
    assert_error(&help, 2, "invalid_configuration", "UNKNOWN_FLAG");
    assert_eq!(help.document["command"], "help");
    assert_eq!(help.document["contract"], "plenora-error-v1");
    for (arguments, code) in cases {
        let rejected = machine(arguments, None);
        assert_error(&rejected, 2, "invalid_configuration", code);
        assert_eq!(rejected.document["command"], arguments[0]);
    }
}

#[test]
fn without_json_format_commands_fail_closed_on_stderr() {
    let cases: [&[&str]; 6] = [
        &["test", "--input", "x.json"],
        &["test", "--input", "x.json", "--format", "text"],
        &["--version"],
        &["capabilities"],
        &[],
        &["--help", "extra"],
    ];
    for arguments in cases {
        let output = run(arguments, None);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let message = String::from_utf8(output.stderr).unwrap();
        assert!(message.starts_with("plenora-rest: "), "{message}");
        assert!(!message.contains("x.json"));
    }
}

// --------------------------------------------------------------- operations

#[test]
fn test_runs_against_a_local_server() {
    let directory = temp_dir("test");
    let (url, server) = serve_once(200, br#"{"ok": true}"#.to_vec());
    let request = write_json(
        &directory,
        "request.json",
        &json!({"schema_version": 1, "operation": "test", "connection": {"url": url}}),
    );
    let config = write_json(&directory, "engine.json", &private_networks());
    let result = machine(
        &[
            "test", "--input", &request, "--config", &config, "--format", "json",
        ],
        None,
    );
    let head = server.join().unwrap();
    assert!(head.starts_with("GET /items "), "{head}");
    assert_eq!(result.exit, 0, "{}", result.document);
    assert_eq!(result.document["command"], "test");
    assert_eq!(
        result.document["contract"],
        "plenora-rest-execution-result-v1"
    );
    assert_eq!(result.document["result"]["status"], "success");
    assert_eq!(result.document["result"]["schema_version"], 1);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_request_can_come_from_standard_input() {
    let (url, server) = serve_once(200, br#"[{"id": 1}, {"id": 2}]"#.to_vec());
    let directory = temp_dir("stdin");
    let config = write_json(&directory, "engine.json", &private_networks());
    let request = json!({"schema_version": 1, "operation": "generate", "connection": {"url": url}});
    let result = machine(
        &[
            "generate", "--input", "-", "--config", &config, "--format", "json",
        ],
        Some(&serde_json::to_vec(&request).unwrap()),
    );
    server.join().unwrap();
    assert_eq!(result.exit, 0, "{}", result.document);
    assert_eq!(result.document["command"], "generate");
    assert_eq!(result.document["result"]["status"], "success");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn download_writes_inside_the_configured_file_root() {
    let directory = temp_dir("download");
    let root = directory.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let body = b"downloaded bytes".to_vec();
    let digest = format!("{:x}", Sha256::digest(&body));
    let (url, server) = serve_once(200, body.clone());
    let request = write_json(
        &directory,
        "request.json",
        &json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": url},
            "input": {"file": {"path": "artifact.bin", "expected_sha256": digest}}
        }),
    );
    let config = write_json(
        &directory,
        "engine.json",
        &json!({
            "allow_private_networks": true,
            "allow_file_transfers": true,
            "file_root": root.to_string_lossy()
        }),
    );
    let result = machine(
        &[
            "download", "--input", &request, "--config", &config, "--format", "json",
        ],
        None,
    );
    server.join().unwrap();
    assert_eq!(result.exit, 0, "{}", result.document);
    assert_eq!(
        result.document["contract"],
        "plenora-rest-file-transfer-result-v1"
    );
    let output = &result.document["result"]["output"];
    assert_eq!(output["type"], "file");
    assert_eq!(output["direction"], "download");
    assert_eq!(output["checksum"]["value"], digest);
    assert_eq!(std::fs::read(root.join("artifact.bin")).unwrap(), body);
    // Il risultato non restituisce il path locale risolto.
    assert!(
        !result
            .document
            .to_string()
            .contains(&*root.to_string_lossy())
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn file_transfers_stay_disabled_without_a_config() {
    let directory = temp_dir("no-transfer");
    let request = write_json(
        &directory,
        "request.json",
        &json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": "https://example.invalid/file"},
            "input": {"file": {"path": "artifact.bin"}}
        }),
    );
    let refused = machine(&["download", "--input", &request, "--format", "json"], None);
    assert_error(&refused, 5, "authorization", "POLICY_VIOLATION");
    std::fs::remove_dir_all(directory).unwrap();
}

// ---------------------------------------------------- errors and exit codes

#[test]
fn private_networks_are_refused_by_default_with_exit_five() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let request = json!({"schema_version": 1, "operation": "test", "connection": {"url": url}});
    let refused = machine(
        &["test", "--input", "-", "--format", "json"],
        Some(&serde_json::to_vec(&request).unwrap()),
    );
    assert_error(&refused, 5, "authorization", "UNSAFE_ADDRESS");
    assert_eq!(
        refused.document["contract"],
        "plenora-rest-execution-result-v1"
    );
    assert!(listener.accept().is_err(), "no connection may be attempted");
}

#[test]
fn a_response_over_its_limit_exits_with_four() {
    let directory = temp_dir("limit");
    let (url, server) = serve_once(200, vec![b' '; 4096]);
    let request = write_json(
        &directory,
        "request.json",
        &json!({"schema_version": 1, "operation": "test", "connection": {"url": url}}),
    );
    let config = write_json(
        &directory,
        "engine.json",
        &json!({"allow_private_networks": true, "max_response_bytes": 16}),
    );
    let limited = machine(
        &[
            "test", "--input", &request, "--config", &config, "--format", "json",
        ],
        None,
    );
    server.join().unwrap();
    assert_error(&limited, 4, "resource_limit", "RESPONSE_TOO_LARGE");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_unsuccessful_status_exits_with_six() {
    let directory = temp_dir("status");
    let (url, server) = serve_once(500, b"{}".to_vec());
    let request = write_json(
        &directory,
        "request.json",
        &json!({"schema_version": 1, "operation": "test", "connection": {"url": url}}),
    );
    let config = write_json(&directory, "engine.json", &private_networks());
    let failed = machine(
        &[
            "test", "--input", &request, "--config", &config, "--format", "json",
        ],
        None,
    );
    server.join().unwrap();
    assert_error(&failed, 6, "execution", "HTTP_STATUS");
    assert_eq!(failed.document["error"]["details"]["http_status"], 500);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_expired_deadline_is_a_typed_timeout() {
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {"url": "https://example.invalid/"},
        "options": {"deadline": "2000-01-01T00:00:00Z"}
    });
    let expired = machine(
        &["test", "--input", "-", "--format", "json"],
        Some(&serde_json::to_vec(&request).unwrap()),
    );
    assert_error(&expired, 5, "timeout", "DEADLINE_EXPIRED");
    assert_eq!(expired.document["error"]["phase"], "validate");
    assert_eq!(expired.document["error"]["remote_effect"], "none");
}

#[test]
fn the_request_operation_must_match_the_command() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let directory = temp_dir("mismatch");
    let request = write_json(
        &directory,
        "request.json",
        &json!({
            "schema_version": 1,
            "operation": "upload",
            "connection": {"url": format!("http://{}/", listener.local_addr().unwrap())}
        }),
    );
    let config = write_json(&directory, "engine.json", &private_networks());
    let mismatch = machine(
        &[
            "test", "--input", &request, "--config", &config, "--format", "json",
        ],
        None,
    );
    assert_error(&mismatch, 2, "invalid_configuration", "OPERATION_MISMATCH");
    assert!(listener.accept().is_err(), "no connection may be attempted");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn unreadable_and_invalid_inputs_are_typed_and_quote_nothing() {
    let directory = temp_dir("inputs");
    let secret = "Bearer-s3cr3t-token";
    let missing = directory.join("missing-request.json");
    let missing = missing.to_string_lossy();

    let absent = machine(&["enrich", "--input", &missing, "--format", "json"], None);
    assert_error(&absent, 5, "io", "INPUT_NOT_FOUND");
    assert!(!absent.document.to_string().contains("missing-request"));

    let broken = directory.join("broken.json");
    std::fs::write(&broken, format!("{{\"schema_version\": 1, \"{secret}\"")).unwrap();
    let invalid = machine(
        &[
            "enrich",
            "--input",
            &broken.to_string_lossy(),
            "--format",
            "json",
        ],
        None,
    );
    assert_error(&invalid, 2, "invalid_configuration", "INVALID_INPUT");
    assert!(!invalid.document.to_string().contains(secret));
    assert!(invalid.document["error"]["details"]["line"].is_u64());

    let unknown_field = json!({
        "schema_version": 1, "operation": "enrich",
        "connection": {"url": "https://example.invalid/"}, secret: true
    });
    let rejected = machine(
        &["enrich", "--input", "-", "--format", "json"],
        Some(&serde_json::to_vec(&unknown_field).unwrap()),
    );
    assert_error(&rejected, 2, "invalid_configuration", "INVALID_INPUT");
    assert!(!rejected.document.to_string().contains(secret));

    let request = write_json(
        &directory,
        "request.json",
        &json!({"schema_version": 1, "operation": "enrich", "connection": {"url": "https://example.invalid/"}}),
    );
    let config = write_json(&directory, "engine.json", &json!({secret: true}));
    let bad_config = machine(
        &[
            "enrich", "--input", &request, "--config", &config, "--format", "json",
        ],
        None,
    );
    assert_error(
        &bad_config,
        2,
        "invalid_configuration",
        "INVALID_CONFIGURATION",
    );
    assert!(!bad_config.document.to_string().contains(secret));

    let missing_config = directory.join("missing-engine.json");
    let absent_config = machine(
        &[
            "enrich",
            "--input",
            &request,
            "--config",
            &missing_config.to_string_lossy(),
            "--format",
            "json",
        ],
        None,
    );
    assert_error(&absent_config, 5, "io", "CONFIG_NOT_FOUND");
    assert!(
        !absent_config
            .document
            .to_string()
            .contains("missing-engine")
    );

    let large = directory.join("large.json");
    std::fs::write(&large, vec![b' '; 1024 * 1024 + 1]).unwrap();
    let too_large = machine(
        &[
            "enrich",
            "--input",
            &request,
            "--config",
            &large.to_string_lossy(),
            "--format",
            "json",
        ],
        None,
    );
    assert_error(&too_large, 4, "resource_limit", "CONFIG_TOO_LARGE");
    assert_eq!(
        too_large.document["error"]["details"]["limit_bytes"],
        1024 * 1024
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_directory_as_input_is_an_io_error() {
    let directory = temp_dir("directory");
    let read = machine(
        &[
            "test",
            "--input",
            &directory.to_string_lossy(),
            "--format",
            "json",
        ],
        None,
    );
    assert_eq!(read.exit, 5, "{}", read.document);
    assert_eq!(read.document["error"]["category"], "io");
    std::fs::remove_dir_all(directory).unwrap();
}

/// Ctrl-C a operazione avviata: il server accetta la connessione e non
/// risponde, il test manda SIGINT al processo e attende l'errore
/// `cancelled` con exit 130. Solo Unix: su Windows non esiste un modo
/// affidabile di recapitare Ctrl-C a un solo processo figlio da un test; lì
/// la proiezione 130 è coperta dai test di unità della mappatura.
#[cfg(unix)]
#[test]
fn an_interrupt_cancels_the_operation_with_exit_130() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/slow", listener.local_addr().unwrap());
    let directory = temp_dir("cancel");
    let request = write_json(
        &directory,
        "request.json",
        &json!({"schema_version": 1, "operation": "test", "connection": {"url": url}}),
    );
    let config = write_json(&directory, "engine.json", &private_networks());
    let child = binary()
        .args([
            "test", "--input", &request, "--config", &config, "--format", "json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // La connessione accettata prova che l'operazione è in corso, quindi il
    // gestore dei segnali è installato.
    let (mut stream, _) = listener.accept().unwrap();
    let _head = read_head(&mut stream);
    let status = Command::new("sh")
        .args(["-c", &format!("kill -INT {}", child.id())])
        .status()
        .unwrap();
    assert!(status.success());
    let output = child.wait_with_output().unwrap();
    drop(stream);
    let cancelled = machine_output(&output);
    assert_error(&cancelled, 130, "cancelled", "CANCELLED");
    assert_eq!(cancelled.document["error"]["remote_effect"], "unknown");
    std::fs::remove_dir_all(directory).unwrap();
}
