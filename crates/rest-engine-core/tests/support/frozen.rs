//! Independent, versioned transition oracle. No production reducer is imported.
use super::*;

#[path = "remote_private.rs"]
mod remote_private;
#[path = "snapshot.rs"]
mod snapshot;

#[tokio::test(start_paused = true)]
async fn frozen_controls_before_send_and_after_headers() {
    // Keep the runtime runnable: virtual time advances only when this test
    // asks it to, even while the real loopback socket is waiting for readiness.
    let keep_awake = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let request = serde_json::from_value(json!({"schema_version":1,"operation":"test","connection":{"url":format!("http://{}/",listener.local_addr().unwrap())}})).unwrap();
    let result = serde_json::to_value(
        local_engine()
            .execute_with_control(request, ExecutionControl::new(token))
            .await,
    )
    .unwrap();
    assert_eq!(result["errors"][0]["code"], "CANCELLED");
    assert_eq!(result["metrics"]["requests"], 0);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    snapshot::observe("cancel-before-send", result);
    for action in ["cancel", "deadline"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let (arrived, mut arrival) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/json\r\n\r\n{").await.unwrap();
            arrived.send(()).unwrap();
            let mut sink = [0_u8; 1];
            let _ = stream.read(&mut sink).await;
        });
        let token = CancellationToken::new();
        let mut control = ExecutionControl::new(token.clone());
        if action == "deadline" {
            let at = time::OffsetDateTime::now_utc() + time::Duration::seconds(60);
            control = control
                .with_deadline(&format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                    at.year(),
                    u8::from(at.month()),
                    at.day(),
                    at.hour(),
                    at.minute(),
                    at.second()
                ))
                .unwrap();
        }
        let request = serde_json::from_value(json!({"schema_version":1,"operation":"test","connection":{"url":url,"request":{"timeout_ms":120000}}})).unwrap();
        let execution =
            tokio::spawn(
                async move { local_engine().execute_with_control(request, control).await },
            );
        let watchdog = std::time::Instant::now();
        while arrival.try_recv().is_err() {
            assert!(
                watchdog.elapsed() < Duration::from_secs(5),
                "server never received the request"
            );
            tokio::task::yield_now().await;
        }
        if action == "cancel" {
            token.cancel();
        } else {
            tokio::time::advance(Duration::from_secs(61)).await;
        }
        while !execution.is_finished() {
            assert!(
                watchdog.elapsed() < Duration::from_secs(5),
                "control did not stop execution"
            );
            tokio::task::yield_now().await;
        }
        let result = serde_json::to_value(execution.await.unwrap()).unwrap();
        assert_eq!(
            result["errors"][0]["code"],
            if action == "cancel" {
                "CANCELLED"
            } else {
                "TIMEOUT"
            }
        );
        assert_eq!(result["metrics"]["requests"], 1);
        assert_eq!(result["metrics"]["retries"], 0);
        snapshot::observe(&format!("{action}-after-headers"), result);
        server.abort();
    }
    keep_awake.abort();
}

#[tokio::test]
async fn frozen_retry_matrix_buffered_and_download() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../../tests/frozen/0.4.0/retry.json")).unwrap();
    for case in fixture["network"].as_array().unwrap() {
        for operation in ["test", "download"] {
            let header = match case["header"].as_str().unwrap() {
                "0" => "0",
                "1" => "1",
                _ => panic!("unsupported fixture header"),
            };
            let responses = case["statuses"]
                .as_array()
                .unwrap()
                .iter()
                .map(|status| {
                    (
                        u16::try_from(status.as_u64().unwrap()).unwrap(),
                        r#"{"ok":true}"#,
                        vec![("Retry-After", header)],
                    )
                })
                .collect();
            let (url, server, observed) = recorded_server(responses).await;
            let directory = transfer_directory("frozen-retry");
            let request = json!({
                "schema_version":1,"operation":operation,
                "connection":{"url":url,"method":case["method"],"retry":{"max_attempts":case["max_attempts"],"backoff_base_ms":0,"max_retry_after_ms":0,"respect_retry_after":case["respect"]}},
                "input":if operation == "download" { json!({"file":{"path":"artifact.bin"}}) } else { json!({}) }
            });
            let result = timeout(
                Duration::from_secs(5),
                execute(&transfer_engine(&directory, 4096, 4096), request),
            )
            .await
            .unwrap();
            assert_eq!(
                observed.lock().unwrap().len(),
                case["statuses"].as_array().unwrap().len()
            );
            assert_eq!(
                result["metrics"]["requests"],
                case["statuses"].as_array().unwrap().len()
            );
            assert_eq!(
                result["status"] == "success",
                case["success"].as_bool().unwrap(),
                "{result}"
            );
            timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            snapshot::observe(
                &format!("retry-{}-{operation}", case["id"].as_str().unwrap()),
                result,
            );
            let mut files = std::fs::read_dir(&directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            files.sort();
            if operation == "download" && case["success"] == true {
                assert_eq!(files, ["artifact.bin"]);
                let bytes = std::fs::read(directory.join("artifact.bin")).unwrap();
                assert_eq!(bytes, br#"{"ok":true}"#);
                snapshot::observe(
                    &format!("artifact-{}", case["id"].as_str().unwrap()),
                    json!({"files":files,"bytes":bytes}),
                );
            } else {
                assert!(
                    files.is_empty(),
                    "failed attempt left an artifact or staging file"
                );
            }
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[tokio::test]
async fn frozen_mixed_records_complete_out_of_order() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../../tests/frozen/0.4.0/mixed.json")).unwrap();
    let mut serial = None;
    for concurrency in [1, 2, 4] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/{{id}}", listener.local_addr().unwrap());
        let replies = fixture["responses"].clone();
        let completed = Arc::new(StdMutex::new(Vec::<String>::new()));
        let completion_log = completed.clone();
        let gate = Arc::new(tokio::sync::Notify::new());
        let server = tokio::spawn(async move {
            let mut workers = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let request = read_request(&mut stream).await;
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .trim_start_matches('/')
                    .to_owned();
                let response = replies[&path].clone();
                assert!(!response.is_null());
                let gate = gate.clone();
                let completed = completion_log.clone();
                workers.push(tokio::spawn(async move {
                    if concurrency > 1 && path == "a" { gate.notified().await; }
                    let body = response["body"].to_string();
                    let reply = format!("HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response["status"], body.len(), body);
                    stream.write_all(reply.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                    let mut byte = [0_u8; 1];
                    assert_eq!(timeout(Duration::from_secs(5), stream.read(&mut byte)).await.unwrap().unwrap(), 0);
                    completed.lock().unwrap().push(path.clone());
                    if path == "b" { gate.notify_one(); }
                }));
            }
            for worker in workers {
                worker.await.unwrap();
            }
        });
        let mut result = timeout(Duration::from_secs(10), execute(&local_engine(), json!({
            "schema_version":1,"operation":"enrich",
            "connection":{"url":url,"method":"GET","parameters":[{"name":"id","mode":"mapped","source":"id","required":true,"location":"path"}]},
            "input":{"records":fixture["input"]},
            "options":{"continue_on_error":true,"enrichment_concurrency":concurrency}
        }))).await.unwrap();
        timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result["status"], "partial");
        assert_eq!(result["output"]["records"], fixture["expected_output"]);
        let errors: Vec<Value> = result["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| json!({"input_index":error["input_index"],"code":error["code"]}))
            .collect();
        assert_eq!(json!(errors), fixture["expected_errors"]);
        assert_eq!(result["metrics"]["requests"], 3);
        assert!(!result.to_string().contains("__CANARY__"));
        let completed = completed.lock().unwrap();
        if concurrency > 1 {
            assert!(
                completed.iter().position(|id| id == "b").unwrap()
                    < completed.iter().position(|id| id == "a").unwrap()
            );
        }
        snapshot::observe(&format!("mixed-{concurrency}"), result.clone());
        result["metrics"]
            .as_object_mut()
            .unwrap()
            .remove("elapsed_ms");
        if let Some(expected) = &serial {
            assert_eq!(&result, expected);
        }
        serial = Some(result);
    }
}

#[tokio::test]
async fn frozen_private_admission_errors() {
    let sentinel = format!("private_core_{:010}", std::process::id());
    let corpus: Value = serde_json::from_str(
        &include_str!("../../../../tests/frozen/0.4.0/private-inputs.json")
            .replace("__CANARY__", &sentinel),
    )
    .unwrap();
    for case in corpus["cases"].as_array().unwrap() {
        let request =
            json!({"schema_version":1,"operation":"test","connection":case["connection"]});
        let public = match local_engine().execute_json(&request.to_string()).await {
            Ok(result) => {
                let parsed: Value = serde_json::from_str(&result).unwrap();
                assert_eq!(parsed["status"], "failed");
                assert_eq!(parsed["metrics"]["requests"], 0);
                result
            }
            Err(error) => {
                assert!(!format!("{error:?} {error}").contains(&sentinel));
                serde_json::to_string(&error.payload()).unwrap()
            }
        };
        assert!(!public.contains(&sentinel));
        snapshot::observe(
            &format!("private-core-{}", case["id"].as_str().unwrap()),
            serde_json::from_str(&public).unwrap(),
        );
    }
}

fn corpus() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/frozen/0.4.0/transitions.json"
    ))
    .unwrap()
}

async fn fixture_server(response: &Value) -> (String, JoinHandle<()>, Arc<StdMutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let body = response["body"].as_str().unwrap();
    let wire = format!(
        "HTTP/1.1 {} Fixture\r\nLocation: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response["status"].as_u64().unwrap(),
        response["location"].as_str().unwrap(),
        body.len(),
        body
    );
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let requests = observed.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let request = read_request(&mut stream).await;
        requests.lock().unwrap().push(request);
        stream.write_all(wire.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    (url, server, observed)
}

fn axes(result: &Value, indexed: bool) -> Value {
    let errors: Vec<Value> = result["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| {
            let mut value = json!({"code":error["code"], "phase":error["phase"],
            "remote_effect":error["remote_effect"], "retry":error["retry"]["kind"]});
            if indexed {
                value["input_index"] = error["input_index"].clone();
            }
            value
        })
        .collect();
    json!({"requests":result["metrics"]["requests"], "errors":errors})
}

#[tokio::test]
async fn frozen_transition_oracle() {
    for case in corpus()["cases"].as_array().unwrap() {
        // Keep the listening socket alive through completion: a forbidden send
        // is observed independently of the engine's own request counter.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut url = format!("http://{}/", listener.local_addr().unwrap());
        let mut serving = None;
        if !case["response"].is_null() {
            let (remote, server, observed) = fixture_server(&case["response"]).await;
            url = remote;
            serving = Some((server, observed));
        }
        let mut connection = case["connection"].clone();
        connection["url"] = json!(url);
        connection["method"] = json!("GET");
        let engine = Engine::new(EngineConfig {
            allow_private_networks: case["private"].as_bool().unwrap(),
            ..EngineConfig::default()
        });
        let result = timeout(
            Duration::from_secs(5),
            execute(
                &engine,
                json!({
                    "schema_version":1,"operation":"test","connection":connection,
                    "input":case["input"],"options":case["options"]
                }),
            ),
        )
        .await
        .expect("transition did not finish");
        assert_eq!(axes(&result, false), case["expected"], "{}", case["id"]);
        snapshot::observe(case["id"].as_str().unwrap(), result.clone());
        if let Some((server, observed)) = serving {
            timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(observed.lock().unwrap().len(), 1);
        } else {
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
}

#[tokio::test]
async fn frozen_serial_concurrent_same_corpus() {
    let fixture = corpus();
    let mut previous = None;
    for concurrency in [1, 2, 4] {
        let redirect = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == "redirect-after-send")
            .unwrap();
        let (url, server, observed) = fixture_server(&redirect["response"]).await;
        let mut result = execute(&local_engine(), json!({
            "schema_version":1,"operation":"enrich",
            "connection":{"url":url,"method":"GET",
                "parameters":[{"name":"id","mode":"mapped","source":"id","required":true,"location":"query"}],
                "request":{"allow_redirects":true,"max_redirects":3}},
            "input":{"records":fixture["records"]["input"]},
            "options":{"continue_on_error":true,"enrichment_concurrency":concurrency}
        })).await;
        timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.lock().unwrap().len(), 1);
        assert_eq!(axes(&result, true), fixture["records"]["expected"]);
        snapshot::observe(&format!("record-errors-{concurrency}"), result.clone());
        // Only the documented wall-clock measurement is nondeterministic.
        result["metrics"]
            .as_object_mut()
            .unwrap()
            .remove("elapsed_ms");
        if let Some(expected) = previous {
            assert_eq!(result, expected);
        }
        previous = Some(result);
    }
}

#[tokio::test]
async fn frozen_batch_has_its_own_wire_oracle() {
    let fixture = corpus();
    let batch = &fixture["batch"];
    let bodies: Vec<String> = batch["bodies"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::to_string)
        .collect();
    let replies = bodies
        .iter()
        .map(|body| (200, body.as_bytes().to_vec(), vec![]))
        .collect();
    let (url, server, observed) = owned_recorded_server(replies).await;
    let result = execute(&local_engine(), json!({
        "schema_version":1,"operation":"enrich",
        "connection":{"url":url,"method":"POST","request":{"body_type":"json"},
            "response":{"output_mapping":[{"path":"name","column":"remote_name"}]},
            "batch":{"enabled":true,"max_size":2,"input_key":"items","input_format":"array","output_path":"results"}},
        "input":{"records":batch["input"]}
    })).await;
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["records"], batch["output"]);
    let observed = observed.lock().unwrap();
    assert_eq!(result["metrics"]["requests"], observed.len());
    let wire: Vec<Value> = observed
        .iter()
        .map(|request| serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap())
        .collect();
    assert_eq!(json!(wire), batch["wire"]);
    snapshot::observe("batch", result);
    snapshot::observe("batch-wire", json!(wire));
}
