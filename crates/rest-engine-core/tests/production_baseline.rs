use plenora_rest_core::{Engine, EngineConfig};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, timeout},
};

const BASELINE_REQUESTS: usize = 32;
const BASELINE_CONCURRENCY: usize = 8;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_concurrency_baseline_preserves_order_and_limits_parallelism() {
    let (base_url, server, peak) =
        delayed_json_server(BASELINE_REQUESTS, Duration::from_millis(20)).await;
    let records = (0..BASELINE_REQUESTS)
        .map(|id| json!({"id": id}))
        .collect::<Vec<_>>();
    let request = json!({
        "schema_version": 1,
        "operation": "enrich",
        "connection": {
            "url": format!("{base_url}records/{{id}}"),
            "method": "GET",
            "parameters": [{
                "name": "id",
                "mode": "mapped",
                "source": "id",
                "required": true,
                "location": "path"
            }]
        },
        "input": {"records": records},
        "options": {
            "continue_on_error": true,
            "enrichment_concurrency": BASELINE_CONCURRENCY
        }
    });

    let result = timeout(Duration::from_secs(10), execute(&local_engine(), request))
        .await
        .expect("bounded concurrency baseline timed out");
    timeout(Duration::from_secs(5), server)
        .await
        .expect("bounded concurrency server did not finish")
        .unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["metrics"]["requests"], BASELINE_REQUESTS);
    let output = result["output"]["records"].as_array().unwrap();
    assert_eq!(output.len(), BASELINE_REQUESTS);
    for (id, record) in output.iter().enumerate() {
        assert_eq!(record["id"], id);
        assert_eq!(record["remote"], id);
    }

    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak > 1,
        "baseline did not exercise parallel requests: peak={observed_peak}"
    );
    assert!(
        observed_peak <= BASELINE_CONCURRENCY,
        "configured concurrency was exceeded: peak={observed_peak}"
    );
}

#[tokio::test]
async fn transient_fault_baseline_recovers_without_request_amplification() {
    let (url, server) = transient_fault_server().await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "retry": {
                "max_attempts": 3,
                "backoff_base_ms": 0,
                "retry_on_status": [429, 503]
            }
        }
    });

    let result = timeout(Duration::from_secs(5), execute(&local_engine(), request))
        .await
        .expect("transient fault baseline timed out");
    timeout(Duration::from_secs(5), server)
        .await
        .expect("transient fault server did not finish")
        .unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["value"], json!({"ok": true}));
    assert_eq!(result["metrics"]["requests"], 3);
    assert_eq!(result["metrics"]["retries"], 2);
}

#[tokio::test]
async fn streaming_baseline_moves_multi_megabyte_payload_outside_memory_limits() {
    let directory = transfer_directory("production-baseline");
    let destination = directory.join("payload.bin");
    let body = vec![b'p'; 2 * 1024 * 1024];
    let digest = sha256(&body);
    let (url, server) = streaming_server(body.clone(), 16 * 1024).await;
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_file_transfers: true,
        file_root: Some(directory.to_string_lossy().into_owned()),
        max_file_transfer_bytes: 4 * 1024 * 1024,
        max_request_bytes: 1024,
        max_response_bytes: 1024,
        ..EngineConfig::default()
    });
    let request = json!({
        "schema_version": 1,
        "operation": "download",
        "connection": {"url": url, "method": "GET"},
        "input": {
            "file": {
                "path": "payload.bin",
                "expected_sha256": digest
            }
        }
    });

    let result = timeout(Duration::from_secs(15), execute(&engine, request))
        .await
        .expect("streaming baseline timed out");
    timeout(Duration::from_secs(5), server)
        .await
        .expect("streaming server did not finish")
        .unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["bytes_transferred"], body.len());
    assert_eq!(result["output"]["checksum"]["value"], sha256(&body));
    assert_eq!(result["metrics"]["bytes_downloaded"], body.len());
    assert_eq!(fs::read(&destination).await.unwrap(), body);
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

fn local_engine() -> Engine {
    Engine::new(EngineConfig {
        allow_private_networks: true,
        ..EngineConfig::default()
    })
}

async fn execute(engine: &Engine, request: Value) -> Value {
    serde_json::from_str(&engine.execute_json(&request.to_string()).await.unwrap()).unwrap()
}

async fn delayed_json_server(
    requests: usize,
    delay: Duration,
) -> (String, JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let task_peak = peak.clone();
    let task = tokio::spawn(async move {
        let mut handlers = Vec::with_capacity(requests);
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().await.unwrap();
            let active = active.clone();
            let peak = task_peak.clone();
            handlers.push(tokio::spawn(async move {
                let request = read_request(&mut stream).await;
                let id = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|path| path.split('?').next())
                    .and_then(|path| path.rsplit('/').next())
                    .and_then(|value| value.parse::<usize>().ok())
                    .expect("request path must end with a numeric id");
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                sleep(delay).await;
                let body = format!(r#"{{"remote":{id}}}"#);
                write_response(&mut stream, 200, "OK", &body, &[]).await;
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
    });
    (format!("http://{address}/"), task, peak)
}

async fn transient_fault_server() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let responses = [
            (503, "Service Unavailable", r#"{"error":"busy"}"#),
            (429, "Too Many Requests", r#"{"error":"limited"}"#),
            (200, "OK", r#"{"ok":true}"#),
        ];
        for (status, reason, body) in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            write_response(&mut stream, status, reason, body, &[("Retry-After", "0")]).await;
        }
    });
    (format!("http://{address}/"), task)
}

async fn streaming_server(body: Vec<u8>, chunk_size: usize) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        for chunk in body.chunks(chunk_size) {
            stream.write_all(chunk).await.unwrap();
            tokio::task::yield_now().await;
        }
        stream.shutdown().await.unwrap();
    });
    (format!("http://{address}/"), task)
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
    headers: &[(&str, &str)],
) {
    let extra_headers = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

async fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 2048];
        let read = stream.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|value| value == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn transfer_directory(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "rest-engine-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn sha256(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

async fn assert_no_partial_files(directory: &Path) {
    let mut entries = fs::read_dir(directory).await.unwrap();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        assert!(
            !entry.file_name().to_string_lossy().ends_with(".part"),
            "partial download was not cleaned up"
        );
    }
}
