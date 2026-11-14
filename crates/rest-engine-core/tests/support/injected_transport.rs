mod injected_transport {
    use crate::injection::{PROBE, Point, Probe};
    use crate::{CancellationToken, Engine, EngineConfig, ExecutionControl};
    use serde_json::{Value, json};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };
    mod snapshot {
        include!("snapshot.rs");
    }

    fn corpus() -> Value {
        serde_json::from_str(include_str!(
            "../../../../tests/frozen/0.4.0/io-branches.json"
        ))
        .unwrap()
    }

    fn artifact_conforms(files: &[String], bytes: &Value, published: bool) -> bool {
        let rule = &corpus()["control_artifact_contract"];
        let state = if files.is_empty() && bytes.is_null() {
            "absent"
        } else if files == ["payload"]
            && *bytes == json!(rule["complete_body"].as_str().unwrap().as_bytes())
        {
            "complete"
        } else {
            return false;
        };
        if published {
            rule["required_after_publication_confirmation"] == state
        } else {
            rule["allowed_before_publication_confirmation"]
                .as_array()
                .unwrap()
                .contains(&json!(state))
        }
    }

    #[test]
    fn frozen_artifact_contract_accepts_only_complete_publication() {
        let complete = json!(
            corpus()["control_artifact_contract"]["complete_body"]
                .as_str()
                .unwrap()
                .as_bytes()
        );
        assert!(artifact_conforms(&[], &Value::Null, false));
        assert!(artifact_conforms(&["payload".into()], &complete, false));
        assert!(artifact_conforms(&["payload".into()], &complete, true));
        assert!(!artifact_conforms(&[], &Value::Null, true));
        assert!(!artifact_conforms(
            &["payload".into()],
            &json!([123]),
            false
        ));
        assert!(!artifact_conforms(
            &["payload".into(), "staging".into()],
            &complete,
            false
        ));
    }

    async fn server(
        statuses: Vec<u16>,
        body: String,
    ) -> (String, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let task = tokio::spawn(async move {
            for status in statuses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    assert_eq!(stream.read(&mut byte).await.unwrap(), 1);
                    request.push(byte[0]);
                }
                observed.fetch_add(1, Ordering::SeqCst);
                if status == 0 {
                    stream.shutdown().await.unwrap();
                    continue;
                }
                if status == 1 {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\nx",
                        )
                        .await
                        .unwrap();
                    stream.shutdown().await.unwrap();
                    continue;
                }
                let reply = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        (url, task, count)
    }

    async fn until(mut condition: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "expected injected boundary was not reached"
            );
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn frozen_io_branch_table_is_exhaustive_for_declared_ports() {
        let observed: Vec<String> = Point::ALL.iter().map(|p| format!("{p:?}")).collect();
        let expected: Vec<String> = corpus()["branches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["point"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(observed, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn frozen_controls_visit_every_declared_branch() {
        use crate::injection::Action;
        let origin = tokio::time::Instant::now();
        let real_origin = std::time::Instant::now();
        let awake = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let selected = std::env::var("SEMPL_CONTROL_CASE").ok();
        let contract = corpus();
        let mut exercised = 0;
        for point in Point::ALL {
            for action in ["cancel", "deadline"] {
                if selected
                    .as_ref()
                    .is_some_and(|id| id != &format!("{point:?}-{action}"))
                {
                    continue;
                }
                exercised += 1;
                let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "../../target/control-{}-{point:?}-{action}",
                    std::process::id()
                ));
                std::fs::create_dir(&directory).unwrap();
                let polling = *point == Point::Polling;
                let body = if polling {
                    r#"{"id":"job-1","status":"queued"}"#
                } else {
                    contract["control_artifact_contract"]["complete_body"]
                        .as_str()
                        .unwrap()
                };
                let warm = matches!(
                    point,
                    Point::CacheHit
                        | Point::CacheRevalidate
                        | Point::CircuitReject
                        | Point::CircuitProbe
                        | Point::OAuthCached
                );
                let statuses = match point {
                    Point::CacheHit => vec![200],
                    Point::CacheRevalidate => vec![200, 304],
                    Point::CircuitReject | Point::CircuitProbe => vec![503, 200],
                    Point::OAuthCached => vec![200, 200, 200],
                    _ => vec![if polling { 202 } else { 200 }, 200],
                };
                let body = if matches!(point, Point::OAuthRead | Point::OAuthCached) {
                    r#"{"access_token":"PRIVATE_TOKEN","token_type":"bearer","expires_in":3600}"#
                } else {
                    body
                };
                let (url, task, _count) = server(statuses, body.to_owned()).await;
                let engine = Engine::new(EngineConfig {
                    allow_private_networks: true,
                    allow_file_transfers: true,
                    allow_cookie_store: true,
                    file_root: Some(directory.to_str().unwrap().to_owned()),
                    ..EngineConfig::default()
                });
                let mut connection = json!({"url":url,"request":{"timeout_ms":120000}});
                let operation = match point {
                    Point::Prepare => "test",
                    Point::OAuth | Point::OAuthRead | Point::OAuthCached => {
                        connection["auth"] = json!({"type":"oauth2_client_credentials","token_url":url,"client_id":"client","client_secret":"PRIVATE_CREDENTIAL"});
                        "test"
                    }
                    Point::Polling => {
                        connection["polling"] = json!({"url_template":url,"status_path":"status","interval_ms":1000,"max_attempts":2});
                        "test"
                    }
                    Point::Cache | Point::CacheHit | Point::CacheRevalidate => {
                        connection["cache"] = json!({"enabled":true,"fresh_for_ms":if *point == Point::CacheRevalidate {0} else {60000}});
                        "test"
                    }
                    Point::Circuit | Point::CircuitReject | Point::CircuitProbe => {
                        connection["circuit_breaker"] = json!({"enabled":true,"group":"frozen","failure_threshold":1,"recovery_timeout_ms":if *point == Point::CircuitProbe {0} else {120000}});
                        "test"
                    }
                    Point::Cookie => {
                        connection["cookies"] =
                            json!({"session":engine.open_cookie_session().await.unwrap()});
                        "test"
                    }
                    _ => "download",
                };
                let token = CancellationToken::new();
                let mut control = ExecutionControl::new(token.clone());
                if action == "deadline" {
                    let offset = i64::try_from(origin.elapsed().as_millis()).unwrap()
                        - i64::try_from(real_origin.elapsed().as_millis()).unwrap()
                        + 60000;
                    let at = time::OffsetDateTime::now_utc() + time::Duration::milliseconds(offset);
                    control = control
                        .with_deadline(&format!(
                            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                            at.year(),
                            u8::from(at.month()),
                            at.day(),
                            at.hour(),
                            at.minute(),
                            at.second(),
                            at.millisecond()
                        ))
                        .unwrap();
                }
                let (waits, _) = mpsc::unbounded_channel();
                let probe = Arc::new(Probe {
                    point: Some(*point),
                    fired: AtomicBool::new(false),
                    waits,
                    action: Some(if action == "cancel" {
                        Action::Cancel(token)
                    } else {
                        Action::Deadline
                    }),
                });
                let request: crate::ExecutionRequest = serde_json::from_value(json!({"schema_version":1,"operation":operation,"connection":connection,
                    "input":if operation == "download" { json!({"file":{"path":"payload","overwrite":*point == Point::Rename}}) } else { json!({}) }})).unwrap();
                if warm {
                    let warm_result = engine.execute(request.clone()).await;
                    assert_eq!(
                        warm_result.metrics.requests,
                        if *point == Point::OAuthCached { 2 } else { 1 }
                    );
                }
                let observing = probe.clone();
                let execution = tokio::spawn(PROBE.scope(probe, async move {
                    engine.execute_with_control(request, control).await
                }));
                until(|| execution.is_finished()).await;
                let result = serde_json::to_value(execution.await.unwrap()).unwrap();
                task.abort();
                assert!(
                    observing.fired.load(Ordering::SeqCst),
                    "branch {point:?} was not reached: {result}"
                );
                assert!(
                    !result.to_string().contains("PRIVATE_"),
                    "assertion failed: {point:?}/{action}: {result}"
                );
                // A synchronous terminal region can finish before the outer
                // select is polled again. Preserve that observable outcome too.
                assert!(
                    result["status"] == "success"
                        || (*point == Point::CircuitReject
                            && result["errors"][0]["code"] == "CIRCUIT_OPEN")
                        || result["errors"][0]["code"]
                            == if action == "cancel" {
                                "CANCELLED"
                            } else {
                                "TIMEOUT"
                            },
                    "assertion failed: {point:?}: {result}"
                );
                // Observe files separately from the cooperative control result.
                // read_dir is not a barrier for a detached filesystem syscall.
                let _ = tokio::fs::read_dir(&directory).await.unwrap();
                let mut files: Vec<_> = std::fs::read_dir(&directory)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect();
                files.sort();
                assert!(
                    files.iter().all(|name| name == "payload"),
                    "assertion failed: leaked staging at {point:?}"
                );
                let artifact = if files.is_empty() {
                    Value::Null
                } else {
                    json!(std::fs::read(directory.join("payload")).unwrap())
                };
                let raw = json!({"files":files,"bytes":artifact});
                let evidence = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/frozen/control-artifacts")
                    .join(std::env::consts::OS);
                std::fs::create_dir_all(&evidence).unwrap();
                std::fs::write(
                    evidence.join(format!("{point:?}-{action}.json")),
                    serde_json::to_vec_pretty(&json!({"artifact":raw,"result":result})).unwrap(),
                )
                .unwrap();
                let expected_artifact = if operation == "download" {
                    assert!(
                        artifact_conforms(
                            &files,
                            &artifact,
                            *point == Point::Cleanup || result["status"] == "success"
                        ),
                        "assertion failed: artifact contract at {point:?}/{action}: {raw}"
                    );
                    corpus()["control_artifact_contract"].clone()
                } else {
                    assert!(files.is_empty() && artifact.is_null());
                    raw
                };
                snapshot::observe(&format!("control-{point:?}-{action}"), result);
                snapshot::observe(
                    &format!("control-artifacts-{point:?}-{action}"),
                    expected_artifact,
                );
                std::fs::remove_dir_all(directory).unwrap();
            }
        }
        assert!(exercised > 0, "unknown control case");
        awake.abort();
    }

    #[tokio::test]
    async fn frozen_io_faults_preserve_publication_and_redaction() {
        let corpus = corpus();
        for (point, row) in Point::ALL
            .iter()
            .zip(corpus["branches"].as_array().unwrap())
            .filter(|(_, row)| !row["code"].is_null())
        {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target")
                .canonicalize()
                .unwrap();
            let directory = root.join(format!("injected-io-{}-{point:?}", std::process::id()));
            std::fs::create_dir(&directory).unwrap();
            let (url, task, count) =
                server(vec![200], corpus["artifact"].as_str().unwrap().to_owned()).await;
            let engine = Engine::new(EngineConfig {
                allow_private_networks: true,
                allow_file_transfers: true,
                file_root: Some(directory.to_str().unwrap().to_owned()),
                ..EngineConfig::default()
            });
            let (waits, _) = mpsc::unbounded_channel();
            let probe = Arc::new(Probe {
                point: Some(*point),
                fired: AtomicBool::new(false),
                waits,
                action: None,
            });
            let request = serde_json::from_value(
                json!({"schema_version":1,"operation":"download","connection":{"url":url},
                "input":{"file":{"path":"PRIVATE_PATH_SENTINEL","overwrite":row["overwrite"]}}}),
            )
            .unwrap();
            let result =
                serde_json::to_value(PROBE.scope(probe.clone(), engine.execute(request)).await)
                    .unwrap();
            task.abort();
            assert!(probe.fired.load(Ordering::SeqCst));
            assert_eq!(
                result["errors"][0]["code"], row["code"],
                "{point:?}: {result}"
            );
            assert_eq!(result["metrics"]["requests"], row["requests"]);
            assert_eq!(json!(count.load(Ordering::SeqCst)), row["requests"]);
            assert!(!result.to_string().contains("PRIVATE_"));
            let published = directory.join("PRIVATE_PATH_SENTINEL").exists();
            assert_eq!(published, row["published"].as_bool().unwrap());
            let files = std::fs::read_dir(&directory).unwrap().count();
            assert_eq!(files, usize::from(published), "staging leak at {point:?}");
            if published {
                assert_eq!(
                    std::fs::read_to_string(directory.join("PRIVATE_PATH_SENTINEL")).unwrap(),
                    corpus["artifact"]
                );
            }
            snapshot::observe(&format!("io-fault-{point:?}"), result);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn frozen_internal_transport_waits_use_injected_clock() {
        let awake = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        for operation in ["test", "download"] {
            for (mode, failure) in [("status", 503), ("connect", 0), ("body", 1)] {
                let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "../../target/injected-wait-{}-{operation}-{mode}",
                    std::process::id()
                ));
                std::fs::create_dir(&directory).unwrap();
                let (url, server, count) =
                    server(vec![failure, failure, 200], r#"{"ok":true}"#.to_owned()).await;
                let engine = Engine::new(EngineConfig {
                    allow_private_networks: true,
                    allow_file_transfers: true,
                    file_root: Some(directory.to_str().unwrap().to_owned()),
                    ..EngineConfig::default()
                });
                let request = serde_json::from_value(json!({"schema_version":1,"operation":operation,
                "connection":{"url":url,"retry":{"max_attempts":3,"backoff_base_ms":250,"backoff_factor":2}},
                "input":if operation == "download" { json!({"file":{"path":"payload"}}) } else { json!({}) }})).unwrap();
                let (waits, mut received) = mpsc::unbounded_channel();
                let probe = Arc::new(Probe {
                    point: None,
                    fired: AtomicBool::new(false),
                    waits,
                    action: None,
                });
                let token = CancellationToken::new();
                let execution = tokio::spawn(PROBE.scope(probe, async move {
                    engine
                        .execute_with_control(request, ExecutionControl::new(token))
                        .await
                }));
                let mut observed = Vec::new();
                for (index, expected) in corpus()["waits_ms"].as_array().unwrap().iter().enumerate()
                {
                    let mut delay = None;
                    until(|| {
                        if let Ok(value) = received.try_recv() {
                            delay = Some(value);
                            true
                        } else {
                            false
                        }
                    })
                    .await;
                    let delay = delay.unwrap();
                    assert_eq!(json!(delay), *expected);
                    assert_eq!(count.load(Ordering::SeqCst), index + 1);
                    tokio::time::advance(Duration::from_millis(delay - 1)).await;
                    for _ in 0..32 {
                        tokio::task::yield_now().await;
                    }
                    assert_eq!(count.load(Ordering::SeqCst), index + 1, "early retry");
                    tokio::time::advance(Duration::from_millis(2)).await;
                    observed.push(delay);
                }
                until(|| execution.is_finished()).await;
                let result = serde_json::to_value(execution.await.unwrap()).unwrap();
                assert_eq!(result["status"], "success");
                assert_eq!(count.load(Ordering::SeqCst), 3);
                server.await.unwrap();
                snapshot::observe(
                    &format!("internal-waits-{operation}-{mode}"),
                    json!(observed),
                );
                snapshot::observe(&format!("internal-wait-result-{operation}-{mode}"), result);
                std::fs::remove_dir_all(directory).unwrap();
            }
        }
        awake.abort();
    }
}
