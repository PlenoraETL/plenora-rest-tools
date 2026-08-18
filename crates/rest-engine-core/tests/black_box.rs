use rest_engine_core::{Engine, EngineConfig};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

#[tokio::test]
async fn blocks_loopback_by_default() {
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {"url": "http://127.0.0.1:9/", "method": "GET"}
    });

    let result: Value = serde_json::from_str(
        &Engine::default()
            .execute_json(&request.to_string())
            .await
            .unwrap(),
    )
    .unwrap();

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "unsafe_address");
}

#[tokio::test]
async fn test_operation_returns_json_through_the_stable_contract() {
    let (url, server) = server(vec![(200, r#"{"ok":true}"#)]).await;
    let engine = local_engine();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {"url": url, "method": "GET"}
    });

    let result = execute(&engine, request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["type"], "json");
    assert_eq!(result["output"]["value"]["ok"], true);
    assert_eq!(result["metrics"]["requests"], 1);
}

#[tokio::test]
async fn head_and_options_support_empty_success_responses_and_metadata() {
    let (url, server, observed) = recorded_server(vec![(204, "", vec![]), (205, "", vec![])]).await;
    let engine = local_engine();
    for (method, expected_status) in [("HEAD", 204), ("OPTIONS", 205)] {
        let result = execute(
            &engine,
            json!({
                "schema_version": 1,
                "operation": "test",
                "connection": {"url": url, "method": method},
                "options": {
                    "capture_response_metadata": true,
                    "response_headers": ["content-type"]
                }
            }),
        )
        .await;
        assert_eq!(result["status"], "success");
        assert_eq!(result["output"]["value"], Value::Null);
        assert_eq!(result["responses"][0]["status"], expected_status);
        assert_eq!(
            result["responses"][0]["headers"]["content-type"],
            "application/json"
        );
    }
    server.await.unwrap();
    let observed = observed.lock().unwrap();
    assert!(observed[0].starts_with("HEAD / "), "{observed:?}");
    assert!(observed[1].starts_with("OPTIONS / "), "{observed:?}");
}

#[tokio::test]
async fn custom_methods_require_an_engine_allowlist() {
    let denied = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {"url": "http://127.0.0.1:9/", "method": "PURGE"}
        }),
    )
    .await;
    assert_eq!(denied["status"], "failed");
    assert_eq!(denied["errors"][0]["code"], "policy_violation");

    let (url, server, observed) = recorded_server(vec![(200, r#"{"purged":true}"#, vec![])]).await;
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allowed_custom_methods: vec!["PURGE".to_owned()],
        ..EngineConfig::default()
    });
    let allowed = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {"url": url, "method": "PURGE"}
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(allowed["status"], "success");
    assert!(observed.lock().unwrap()[0].starts_with("PURGE / "));
}

#[tokio::test]
async fn generate_follows_rfc_link_headers() {
    let (url, server, observed) = recorded_server(vec![
        (
            200,
            r#"{"items":[{"id":1}]}"#,
            vec![("Link", "</?page=2>; rel=\"next alternate\"")],
        ),
        (200, r#"{"items":[{"id":2}]}"#, vec![]),
    ])
    .await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {"records_path": "items"},
                "pagination": {
                    "type": "header_link",
                    "relation": "next",
                    "max_rows": 10,
                    "max_pages": 5
                }
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["records"], json!([{"id": 1}, {"id": 2}]));
    assert_eq!(result["metrics"]["requests"], 2);
    assert!(observed.lock().unwrap()[1].starts_with("GET /?page=2 "));
}

#[tokio::test]
async fn gzip_responses_are_decompressed_before_json_parsing() {
    let gzip = vec![
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 171, 86, 202, 207, 86, 178, 42, 41, 42, 77, 173, 5, 0,
        144, 95, 212, 167, 11, 0, 0, 0,
    ];
    let (url, server) = binary_server(
        200,
        gzip,
        vec![
            ("Content-Type", "application/json"),
            ("Content-Encoding", "gzip"),
        ],
    )
    .await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {"url": url, "method": "GET"}
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["value"], json!({"ok": true}));
}

#[tokio::test]
async fn parameters_can_target_query_headers_cookies_and_json_body() {
    let (url, server, observed) =
        recorded_server(vec![(200, r#"{"accepted":true}"#, vec![])]).await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "POST",
                "parameters": [
                    {
                        "name": "filter",
                        "mode": "fixed",
                        "value": {"active": true, "role": "admin"},
                        "location": "query",
                        "query_serialization": {"style": "deep_object"}
                    },
                    {
                        "name": "tags",
                        "mode": "fixed",
                        "value": ["red", "blue"],
                        "location": "query",
                        "query_serialization": {
                            "style": "pipe_delimited",
                            "explode": false
                        }
                    },
                    {
                        "name": "X-Tenant",
                        "mode": "fixed",
                        "value": "acme",
                        "location": "header"
                    },
                    {
                        "name": "session",
                        "mode": "fixed",
                        "value": "abc123",
                        "location": "cookie"
                    },
                    {
                        "name": "item",
                        "mode": "fixed",
                        "value": {"id": 7},
                        "location": "body"
                    }
                ]
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    let request = observed.lock().unwrap()[0].clone();
    assert!(request.starts_with("POST /?"));
    assert!(request.contains("filter%5Bactive%5D=true"));
    assert!(request.contains("filter%5Brole%5D=admin"));
    assert!(request.contains("tags=red%7Cblue"));
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains("x-tenant: acme"));
    assert!(lower.contains("cookie: session=abc123"));
    assert!(request.contains(r#"{"item":{"id":7}}"#));
}

#[tokio::test]
async fn generate_accepts_ndjson_as_a_record_stream_format() {
    let (url, server) = server(vec![(200, "{\"id\":1}\n{\"id\":2}\n")]).await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {"format": "ndjson"}
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["records"], json!([{"id": 1}, {"id": 2}]));
}

#[tokio::test]
async fn generate_paginates_and_maps_records() {
    let (url, server) = server(vec![
        (
            200,
            r#"{"items":[{"profile":{"id":1}},{"profile":{"id":2}}]}"#,
        ),
        (200, r#"{"items":[{"profile":{"id":3}}]}"#),
    ])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "generate",
        "connection": {
            "url": url,
            "method": "GET",
            "response": {
                "records_path": "items",
                "output_mapping": [{"path": "profile.id", "column": "id"}]
            },
            "pagination": {
                "type": "page",
                "page_size": 2,
                "max_rows": 10
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["metrics"]["requests"], 2);
    assert_eq!(
        result["output"]["records"],
        json!([{"id": 1}, {"id": 2}, {"id": 3}])
    );
}

#[tokio::test]
async fn generate_supports_nested_iteration_and_transforms() {
    let (url, server) = server(vec![(
        200,
        r#"{"data":[{"stat":{"lat":45.1},"prod":[{"var":"B12101","val":[{"val":300.15,"ref":"t1"},{"val":301.15,"ref":"t2"}]}]}]}"#,
    )])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "generate",
        "connection": {
            "url": url,
            "method": "GET",
            "response": {
                "records_path": "data",
                "iterate_on": [
                    {"path": "", "as": "station"},
                    {"path": "prod", "as": "product"},
                    {"path": "val", "as": "measurement"}
                ],
                "output_mapping": [
                    {"path": "station.stat.lat", "column": "lat"},
                    {"path": "product.var", "column": "var"},
                    {"path": "measurement.val", "column": "kelvin"},
                    {"path": "measurement.ref", "column": "timestamp"}
                ],
                "transforms": [{
                    "column": "celsius",
                    "source": "kelvin",
                    "operation": "kelvin_to_celsius",
                    "condition": "var == 'B12101'"
                }]
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["records"].as_array().unwrap().len(), 2);
    assert_eq!(result["output"]["records"][0]["lat"], 45.1);
    assert_eq!(result["output"]["records"][0]["celsius"], 27.0);
    assert_eq!(result["output"]["records"][1]["timestamp"], "t2");
}

#[tokio::test]
async fn enrich_preserves_input_and_adds_mapped_fields() {
    let (base_url, server) = server(vec![
        (200, r#"{"profile":{"name":"Ada"}}"#),
        (200, r#"{"profile":{"name":"Grace"}}"#),
    ])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "enrich",
        "connection": {
            "url": format!("{base_url}users/{{user_id}}"),
            "method": "GET",
            "parameters": [{
                "name": "user_id",
                "mode": "mapped",
                "source": "id",
                "required": true
            }],
            "response": {
                "output_mapping": [{"path": "profile.name", "column": "name"}]
            }
        },
        "input": {"records": [{"id": 1}, {"id": 2}]}
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(
        result["output"]["records"],
        json!([{"id": 1, "name": "Ada"}, {"id": 2, "name": "Grace"}])
    );
}

#[tokio::test]
async fn enrich_uses_native_batch_contract() {
    let (url, server, observed) = recorded_server(vec![
        (
            200,
            r#"{"results":[{"name":"Ada"},{"name":"Grace"}]}"#,
            vec![],
        ),
        (200, r#"{"results":[{"name":"Linus"}]}"#, vec![]),
    ])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "enrich",
        "connection": {
            "url": url,
            "method": "POST",
            "request": {"body_type": "json"},
            "response": {
                "output_mapping": [{"path": "name", "column": "remote_name"}]
            },
            "batch": {
                "enabled": true,
                "max_size": 2,
                "input_key": "items",
                "input_format": "array",
                "output_path": "results"
            }
        },
        "input": {"records": [{"id": 1}, {"id": 2}, {"id": 3}]}
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["metrics"]["requests"], 2);
    assert_eq!(
        result["output"]["records"],
        json!([
            {"id": 1, "remote_name": "Ada"},
            {"id": 2, "remote_name": "Grace"},
            {"id": 3, "remote_name": "Linus"}
        ])
    );
    let observed = observed.lock().unwrap();
    assert!(observed[0].contains(r#""items":[{"id":1},{"id":2}]"#));
    assert!(observed[1].contains(r#""items":[{"id":3}]"#));
}

#[tokio::test]
async fn retry_is_owned_and_reported_by_the_engine() {
    let (url, server) = server(vec![(503, r#"{"error":"busy"}"#), (200, r#"{"ok":true}"#)]).await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "retry": {
                "max_attempts": 2,
                "backoff_base_ms": 0,
                "retry_on_status": [503]
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["metrics"]["requests"], 2);
    assert_eq!(result["metrics"]["retries"], 1);
}

#[tokio::test]
async fn oauth_client_credentials_is_cached_inside_the_engine() {
    let (base_url, server, observed) = recorded_server(vec![
        (
            200,
            r#"{"access_token":"engine-token","token_type":"Bearer","expires_in":3600}"#,
            vec![],
        ),
        (200, r#"{"ok":true}"#, vec![]),
        (200, r#"{"ok":true}"#, vec![]),
    ])
    .await;
    let engine = local_engine();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{base_url}resource"),
            "method": "GET",
            "auth": {
                "type": "oauth2_client_credentials",
                "token_url": format!("{base_url}token"),
                "client_id": "client",
                "client_secret": "secret",
                "scope": "read"
            }
        }
    });

    let first = execute(&engine, request.clone()).await;
    let second = execute(&engine, request).await;
    server.await.unwrap();

    assert_eq!(first["status"], "success");
    assert_eq!(first["metrics"]["requests"], 2);
    assert_eq!(first["metrics"]["auth_requests"], 1);
    assert_eq!(second["metrics"]["requests"], 1);
    assert_eq!(second["metrics"]["auth_requests"], 0);
    let requests = observed.lock().unwrap();
    assert!(requests[0].starts_with("POST /token "));
    assert!(requests[0].contains("grant_type=client_credentials"));
    assert!(requests[0].contains("scope=read"));
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("authorization: basic ")
    );
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer engine-token")
    );
    assert!(
        requests[2]
            .to_ascii_lowercase()
            .contains("authorization: bearer engine-token")
    );
}

#[tokio::test]
async fn multipart_is_encoded_entirely_inside_the_engine() {
    let (url, server, observed) =
        recorded_server(vec![(200, r#"{"uploaded":true}"#, vec![])]).await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "POST",
            "request": {"body_type": "multipart"}
        },
        "input": {
            "params": {
                "description": "document",
                "attachment": {
                    "filename": "hello.txt",
                    "content_type": "text/plain",
                    "data_base64": "aGVsbG8="
                }
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    let request = &observed.lock().unwrap()[0];
    assert!(request.contains("name=\"description\""));
    assert!(request.contains("document"));
    assert!(request.contains("name=\"attachment\""));
    assert!(request.contains("filename=\"hello.txt\""));
    assert!(request.contains("Content-Type: text/plain"));
    assert!(request.contains("hello"));
}

#[tokio::test]
async fn polling_returns_only_the_completed_result() {
    let (base_url, server, _) = recorded_server(vec![
        (202, r#"{"accepted":true}"#, vec![("Location", "/jobs/1")]),
        (200, r#"{"status":"running"}"#, vec![]),
        (
            200,
            r#"{"status":"completed","result":{"answer":42}}"#,
            vec![],
        ),
    ])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{base_url}jobs"),
            "method": "POST",
            "polling": {
                "status_path": "status",
                "result_path": "result",
                "interval_ms": 0,
                "max_attempts": 3
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["value"], json!({"answer": 42}));
    assert_eq!(result["metrics"]["requests"], 3);
    assert_eq!(result["metrics"]["poll_requests"], 2);
}

#[tokio::test]
async fn polling_can_follow_header_job_id_and_result_url() {
    let (base_url, server, observed) = recorded_server(vec![
        (202, "", vec![("X-Job-Id", "job/1")]),
        (
            200,
            r#"{"status":"completed","result_url":"{base}/out/{job_id}"}"#,
            vec![],
        ),
        (200, r#"{"items":[{"x":2}]}"#, vec![]),
    ])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{base_url}run"),
            "method": "POST",
            "polling": {
                "url_template": "{base}/jobs/{job_id}",
                "id_header": "X-Job-Id",
                "location_header": null,
                "status_path": "status",
                "result_url_path": "result_url",
                "interval_ms": 0,
                "max_attempts": 2
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["value"], json!({"items": [{"x": 2}]}));
    assert_eq!(result["metrics"]["requests"], 3);
    let observed = observed.lock().unwrap();
    assert!(observed[1].starts_with("GET /jobs/job%2F1 "));
    assert!(observed[2].starts_with("GET /out/job%2F1 "));
}

#[tokio::test]
async fn application_success_rules_have_a_stable_error_code() {
    let (url, server) = server(vec![(
        200,
        r#"{"status":"error","error":{"message":"bad"}}"#,
    )])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "response": {
                "error_path": "error",
                "success_when": {"path": "status", "equals": "ok"}
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "application_error");
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("bad")
    );
}

#[tokio::test]
async fn dangerous_transport_options_are_denied_by_default() {
    for connection in [
        json!({
            "url": "https://example.com",
            "method": "GET",
            "tls": {"verify": false}
        }),
        json!({
            "url": "https://example.com",
            "method": "GET",
            "proxy": {"url": "http://proxy.example.com:8080"}
        }),
    ] {
        let result = execute(
            &Engine::default(),
            json!({
                "schema_version": 1,
                "operation": "test",
                "connection": connection
            }),
        )
        .await;
        assert_eq!(result["status"], "failed");
        assert_eq!(result["errors"][0]["code"], "policy_violation");
    }
}

#[tokio::test]
async fn global_rate_limit_is_visible_in_metrics() {
    let (url, server) = server(vec![
        (200, r#"{"ok":true}"#),
        (200, r#"{"ok":true}"#),
        (200, r#"{"ok":true}"#),
    ])
    .await;
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        requests_per_second: Some(5),
        ..EngineConfig::default()
    });
    let request = json!({
        "schema_version": 1,
        "operation": "enrich",
        "connection": {"url": url, "method": "GET"},
        "input": {"records": [{"id": 1}, {"id": 2}, {"id": 3}]}
    });

    let result = execute(&engine, request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["metrics"]["requests"], 3);
    assert!(result["metrics"]["rate_limit_wait_ms"].as_u64().unwrap() >= 250);
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

async fn server(responses: Vec<(u16, &'static str)>) -> (String, JoinHandle<()>) {
    let responses = responses
        .into_iter()
        .map(|(status, body)| (status, body, Vec::new()))
        .collect();
    let (url, task, _) = recorded_server(responses).await;
    (url, task)
}

type TestResponse = (u16, &'static str, Vec<(&'static str, &'static str)>);

async fn recorded_server(
    responses: Vec<TestResponse>,
) -> (String, JoinHandle<()>, Arc<StdMutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let task_observed = observed.clone();
    let task = tokio::spawn(async move {
        for (status, body, headers) in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            task_observed.lock().unwrap().push(request);
            let reason = match status {
                200 => "OK",
                202 => "Accepted",
                503 => "Service Unavailable",
                _ => "Response",
            };
            let extra_headers: String = headers
                .into_iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    (format!("http://{address}/"), task, observed)
}

async fn binary_server(
    status: u16,
    body: Vec<u8>,
    headers: Vec<(&'static str, &'static str)>,
) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut stream).await;
        let extra_headers: String = headers
            .into_iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let head = format!(
            "HTTP/1.1 {status} OK\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    (format!("http://{address}/"), task)
}

async fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 2_048];
        let read = stream.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|value| value == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
            })
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}
use std::sync::{Arc, Mutex as StdMutex};
