use plenora_rest_core::{
    CancellationToken, CookieSession, Engine, EngineConfig, EngineError, ExecutionControl,
    ExecutionRequest,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Barrier,
    task::JoinHandle,
    time::timeout,
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
    assert_eq!(result["errors"][0]["code"], "UNSAFE_ADDRESS");
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
    assert_eq!(denied["errors"][0]["code"], "POLICY_VIOLATION");

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
    let request = observed.lock().unwrap()[0].clone();
    assert!(request.contains("name=\"description\""));
    assert!(request.contains("document"));
    assert!(request.contains("name=\"attachment\""));
    assert!(request.contains("filename=\"hello.txt\""));
    assert!(request.contains("Content-Type: text/plain"));
    assert!(request.contains("hello"));
}

#[tokio::test]
async fn file_transfers_are_denied_by_default() {
    let result = execute(
        &Engine::default(),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": "http://127.0.0.1:9/", "method": "GET"},
            "input": {"file": {"path": "denied.bin"}}
        }),
    )
    .await;

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "POLICY_VIOLATION");
}

#[tokio::test]
async fn file_root_blocks_escape_and_downloads_do_not_clobber_by_default() {
    let directory = transfer_directory("file-policy");
    let engine = transfer_engine(&directory, 1024, 64);
    let escaped = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": "http://127.0.0.1:9/", "method": "GET"},
            "input": {"file": {"path": "../outside.bin"}}
        }),
    )
    .await;
    assert_eq!(escaped["errors"][0]["code"], "POLICY_VIOLATION");

    let destination = directory.join("existing.bin");
    fs::write(&destination, b"keep-me").await.unwrap();
    let existing = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": "http://127.0.0.1:9/", "method": "GET"},
            "input": {"file": {"path": "existing.bin"}}
        }),
    )
    .await;
    assert_eq!(existing["errors"][0]["code"], "FILE_IO");
    assert_eq!(fs::read(&destination).await.unwrap(), b"keep-me");
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn file_transfers_require_a_configured_file_root() {
    let directory = transfer_directory("no-root");
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_file_transfers: true,
        file_root: None,
        ..EngineConfig::default()
    });
    let absolute = directory.join("escaped.bin");

    for path in [
        Value::String(absolute.to_string_lossy().into_owned()),
        json!("relative.bin"),
    ] {
        let result = execute(
            &engine,
            json!({
                "schema_version": 1,
                "operation": "download",
                "connection": {"url": "http://127.0.0.1:9/", "method": "GET"},
                "input": {"file": {"path": path}}
            }),
        )
        .await;
        assert_eq!(result["status"], "failed");
        assert_eq!(result["errors"][0]["code"], "POLICY_VIOLATION");
    }

    let upload = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "upload",
            "connection": {"url": "http://127.0.0.1:9/", "method": "POST"},
            "input": {"file": {"path": absolute.to_string_lossy()}}
        }),
    )
    .await;
    assert_eq!(upload["errors"][0]["code"], "POLICY_VIOLATION");
    assert!(!absolute.exists());
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn cross_origin_pagination_does_not_forward_credentials() {
    // Two loopback listeners differ by port, so they are distinct origins.
    let (second_url, second_server, second_observed) =
        recorded_server(vec![(200, r#"{"items":[{"id":2}]}"#, vec![])]).await;
    let first_body = format!(r#"{{"items":[{{"id":1}}],"next":"{second_url}"}}"#);
    let (first_url, first_server, first_observed) = owned_recorded_server(vec![(
        200,
        first_body.into_bytes(),
        vec![("Content-Type", "application/json")],
    )])
    .await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": first_url,
                "method": "GET",
                "headers": {"X-Trace-Token": "trace-secret"},
                "auth": {"type": "bearer", "token": "super-secret"},
                "response": {"records_path": "items"},
                "pagination": {
                    "type": "link",
                    "link_path": "next",
                    "max_pages": 2,
                    "allow_cross_origin": true
                }
            }
        }),
    )
    .await;
    first_server.await.unwrap();
    second_server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["records"].as_array().unwrap().len(), 2);

    let first_request = first_observed.lock().unwrap()[0].to_ascii_lowercase();
    assert!(first_request.contains("authorization: bearer super-secret"));
    assert!(first_request.contains("x-trace-token"));

    let second_request = second_observed.lock().unwrap()[0].to_ascii_lowercase();
    assert!(
        !second_request.contains("authorization"),
        "credentials must not follow a pagination link to another origin: {second_request}"
    );
    assert!(
        !second_request.contains("super-secret") && !second_request.contains("trace-secret"),
        "no credential material may reach another origin: {second_request}"
    );
}

#[tokio::test]
async fn cross_origin_pagination_keeps_polling_follow_ups_unauthenticated() {
    // The second page is served by another origin and starts a polling flow.
    // The poll must not re-acquire the credentials from the connection: the
    // page that introduced it is not the origin that owns them.
    let (second_url, second_server, second_observed) = recorded_server(vec![
        (
            200,
            r#"{"status":"pending","poll":"/jobs/9"}"#,
            vec![("Content-Type", "application/json")],
        ),
        (
            200,
            r#"{"status":"completed","items":[{"id":2}]}"#,
            vec![("Content-Type", "application/json")],
        ),
    ])
    .await;
    let first_body =
        format!(r#"{{"status":"completed","items":[{{"id":1}}],"next":"{second_url}"}}"#);
    let (first_url, first_server, _) = owned_recorded_server(vec![(
        200,
        first_body.into_bytes(),
        vec![("Content-Type", "application/json")],
    )])
    .await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": first_url,
                "method": "GET",
                "auth": {"type": "bearer", "token": "super-secret"},
                "response": {"records_path": "items"},
                "polling": {
                    "url_path": "poll",
                    "status_path": "status",
                    "interval_ms": 0,
                    "max_attempts": 3,
                    "allow_cross_origin": true
                },
                "pagination": {
                    "type": "link",
                    "link_path": "next",
                    "max_pages": 2,
                    "allow_cross_origin": true
                }
            }
        }),
    )
    .await;
    first_server.await.unwrap();
    second_server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    let observed = second_observed.lock().unwrap();
    assert_eq!(observed.len(), 2, "expected a page and a poll request");
    for request in observed.iter() {
        let request = request.to_ascii_lowercase();
        assert!(
            !request.contains("authorization") && !request.contains("super-secret"),
            "credentials leaked to a cross-origin follow-up: {request}"
        );
    }
}

#[tokio::test]
async fn cursor_pagination_is_scoped_to_the_first_origin() {
    // A cursor is remote input and placeholders are substituted anywhere in the
    // URL, so cursor pagination must be scoped to the first origin exactly like
    // an explicit link. Two loopback listeners differ by port, which makes them
    // distinct origins without depending on name resolution.
    let (second_url, second_server, second_observed) =
        recorded_server(vec![(200, r#"{"items":[{"id":2}]}"#, vec![])]).await;
    let port_of = |url: &str| {
        url.trim_end_matches('/')
            .rsplit(':')
            .next()
            .expect("a loopback URL always carries a port")
            .to_owned()
    };
    let second_port = port_of(&second_url);
    let first_body = format!(r#"{{"items":[{{"id":1}}],"cursor":"{second_port}"}}"#);
    let (first_url, first_server, first_observed) = owned_recorded_server(vec![(
        200,
        first_body.into_bytes(),
        vec![("Content-Type", "application/json")],
    )])
    .await;
    let first_port = port_of(&first_url);

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": "http://127.0.0.1:{port}/",
                "method": "GET",
                "auth": {"type": "bearer", "token": "super-secret"},
                "static_parameters": {"port": first_port},
                "response": {"records_path": "items"},
                "pagination": {
                    "type": "cursor",
                    "cursor_param": "port",
                    "cursor_path": "cursor",
                    "max_pages": 2
                }
            }
        }),
    )
    .await;
    first_server.await.unwrap();
    second_server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    assert_eq!(result["output"]["records"].as_array().unwrap().len(), 2);

    let first_request = first_observed.lock().unwrap()[0].to_ascii_lowercase();
    assert!(
        first_request.contains("authorization: bearer super-secret"),
        "the first page owns the credentials: {first_request}"
    );
    let second_request = second_observed.lock().unwrap()[0].to_ascii_lowercase();
    assert!(
        !second_request.contains("authorization") && !second_request.contains("super-secret"),
        "a cursor must not carry credentials to another origin: {second_request}"
    );
}

#[tokio::test]
async fn returning_to_the_first_origin_does_not_restore_credentials() {
    // A -> B -> A. The page on B is correctly anonymised, but the page after it
    // is rebuilt from the connection, so without a monotone authorization it
    // would reach A fully authenticated at a path B chose. B never sees the
    // secret, yet it would be deciding which authenticated request A receives.
    //
    // The two bodies reference each other, so both listeners are bound before
    // either starts answering.
    let owner = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let detour = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner_url = format!("http://{}/", owner.local_addr().unwrap());
    let detour_url = format!("http://{}/", detour.local_addr().unwrap());

    let owner_requests = Arc::new(StdMutex::new(Vec::new()));
    let detour_requests = Arc::new(StdMutex::new(Vec::new()));
    let owner_bodies = vec![
        format!(r#"{{"items":[{{"id":1}}],"next":"{detour_url}"}}"#),
        r#"{"items":[{"id":3}]}"#.to_owned(),
    ];
    let detour_bodies = vec![format!(r#"{{"items":[{{"id":2}}],"next":"{owner_url}"}}"#)];

    let owner_server = serve_json(owner, owner_bodies, owner_requests.clone());
    let detour_server = serve_json(detour, detour_bodies, detour_requests.clone());

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": owner_url,
                "method": "GET",
                "auth": {"type": "bearer", "token": "super-secret"},
                "response": {"records_path": "items"},
                "pagination": {
                    "type": "link",
                    "link_path": "next",
                    "max_pages": 3,
                    "allow_cross_origin": true
                }
            }
        }),
    )
    .await;
    owner_server.await.unwrap();
    detour_server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    assert_eq!(result["output"]["records"].as_array().unwrap().len(), 3);

    let owner_requests = owner_requests.lock().unwrap();
    assert_eq!(
        owner_requests.len(),
        2,
        "the owning origin served two pages"
    );
    assert!(
        owner_requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer super-secret"),
        "the first page owns the credentials: {}",
        owner_requests[0]
    );
    assert!(
        !owner_requests[1]
            .to_ascii_lowercase()
            .contains("authorization"),
        "returning to the owning origin must not restore the credentials: {}",
        owner_requests[1]
    );
    let detour_requests = detour_requests.lock().unwrap();
    assert!(
        !detour_requests[0]
            .to_ascii_lowercase()
            .contains("super-secret"),
        "the detour must never see the secret: {}",
        detour_requests[0]
    );
}

#[tokio::test]
async fn cross_origin_pages_keep_the_idempotency_key() {
    // The key is caller generated, not a credential, and it is what enables
    // retrying a non-idempotent method. Dropping it while leaving retries on
    // would silently remove the protection it exists to provide.
    let (second_url, second_server, second_observed) =
        recorded_server(vec![(200, r#"{"items":[{"id":2}]}"#, vec![])]).await;
    let first_body = format!(r#"{{"items":[{{"id":1}}],"next":"{second_url}"}}"#);
    let (first_url, first_server, first_observed) = owned_recorded_server(vec![(
        200,
        first_body.into_bytes(),
        vec![("Content-Type", "application/json")],
    )])
    .await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": first_url,
                "method": "GET",
                "auth": {"type": "bearer", "token": "super-secret"},
                "response": {"records_path": "items"},
                "pagination": {
                    "type": "link",
                    "link_path": "next",
                    "max_pages": 2,
                    "allow_cross_origin": true
                }
            },
            "options": {"idempotency_key": "page-key-1"}
        }),
    )
    .await;
    first_server.await.unwrap();
    second_server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    // Checked on the owning origin first: without this the test would also pass
    // if the header were never emitted at all.
    let first_request = first_observed.lock().unwrap()[0].to_ascii_lowercase();
    assert!(
        first_request.contains("idempotency-key: "),
        "the owning origin must receive the idempotency header: {first_request}"
    );
    let second_request = second_observed.lock().unwrap()[0].to_ascii_lowercase();
    // The engine derives a distinct key per page, so the header carries that
    // derived value rather than the caller's key verbatim; what matters here is
    // that the header survives the cross-origin hop at all.
    assert!(
        second_request.contains("idempotency-key: "),
        "the idempotency key is not a credential and must survive: {second_request}"
    );
    assert!(
        !second_request.contains("authorization"),
        "credentials must still be stripped: {second_request}"
    );
}

#[tokio::test]
async fn a_result_url_back_on_the_first_origin_stays_unauthenticated() {
    // Initial request on A, polling on B, result back on A. The result request
    // is rebuilt from the connection, so without a scope that stays narrowed
    // across both hops it would arrive at A fully authenticated at a URL B
    // chose.
    let owner = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let poller = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner_url = format!("http://{}/", owner.local_addr().unwrap());
    let poller_url = format!("http://{}/", poller.local_addr().unwrap());

    let owner_requests = Arc::new(StdMutex::new(Vec::new()));
    let poller_requests = Arc::new(StdMutex::new(Vec::new()));
    let owner_server = serve_json(
        owner,
        vec![
            format!(r#"{{"status":"pending","poll":"{poller_url}"}}"#),
            r#"{"value":{"done":true}}"#.to_owned(),
        ],
        owner_requests.clone(),
    );
    let poller_server = serve_json(
        poller,
        vec![format!(
            r#"{{"status":"completed","result":"{owner_url}"}}"#
        )],
        poller_requests.clone(),
    );

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": owner_url,
                "method": "GET",
                "auth": {"type": "bearer", "token": "super-secret"},
                "polling": {
                    "url_path": "poll",
                    "status_path": "status",
                    "result_url_path": "result",
                    "interval_ms": 0,
                    "max_attempts": 3,
                    "allow_cross_origin": true
                }
            }
        }),
    )
    .await;
    owner_server.await.unwrap();
    poller_server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    let owner_requests = owner_requests.lock().unwrap();
    assert_eq!(
        owner_requests.len(),
        2,
        "initial request and result request"
    );
    assert!(
        owner_requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer super-secret"),
        "the initial request owns the credentials: {}",
        owner_requests[0]
    );
    assert!(
        !owner_requests[1]
            .to_ascii_lowercase()
            .contains("authorization"),
        "a result URL chosen after a cross-origin poll must not be authenticated: {}",
        owner_requests[1]
    );
}

#[tokio::test]
async fn pagination_keeps_restrictions_introduced_inside_polling() {
    // Page 1 on A polls B, so the authorization is revoked while following that
    // chain. Page 2 is served by A again: the revocation has to have flowed back
    // to the pagination scope, otherwise the page is authenticated again.
    let owner = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let poller = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner_url = format!("http://{}/", owner.local_addr().unwrap());
    let poller_url = format!("http://{}/", poller.local_addr().unwrap());

    let owner_requests = Arc::new(StdMutex::new(Vec::new()));
    let poller_requests = Arc::new(StdMutex::new(Vec::new()));
    let owner_server = serve_json(
        owner,
        vec![
            format!(r#"{{"status":"pending","poll":"{poller_url}"}}"#),
            r#"{"status":"completed","items":[{"id":2}]}"#.to_string(),
        ],
        owner_requests.clone(),
    );
    let poller_server = serve_json(
        poller,
        vec![format!(
            r#"{{"status":"completed","items":[{{"id":1}}],"next":"{owner_url}"}}"#
        )],
        poller_requests.clone(),
    );

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": owner_url,
                "method": "GET",
                "auth": {"type": "bearer", "token": "super-secret"},
                "response": {"records_path": "items"},
                "polling": {
                    "url_path": "poll",
                    "status_path": "status",
                    "interval_ms": 0,
                    "max_attempts": 3,
                    "allow_cross_origin": true
                },
                "pagination": {
                    "type": "link",
                    "link_path": "next",
                    "max_pages": 2,
                    "allow_cross_origin": true
                }
            }
        }),
    )
    .await;
    owner_server.await.unwrap();
    poller_server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    let owner_requests = owner_requests.lock().unwrap();
    assert_eq!(owner_requests.len(), 2, "first page and second page");
    assert!(
        owner_requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer super-secret"),
        "the first page owns the credentials: {}",
        owner_requests[0]
    );
    assert!(
        !owner_requests[1]
            .to_ascii_lowercase()
            .contains("authorization"),
        "a page reached through a cross-origin poll must stay unauthenticated: {}",
        owner_requests[1]
    );
}

#[tokio::test]
async fn a_credential_named_idempotency_header_is_not_forwarded() {
    // The idempotency header name is caller configured; naming it
    // `Authorization` must not turn the allowlist exception into a way of
    // forwarding an auth header to another origin.
    let (second_url, second_server, second_observed) =
        recorded_server(vec![(200, r#"{"items":[{"id":2}]}"#, vec![])]).await;
    let first_body = format!(r#"{{"items":[{{"id":1}}],"next":"{second_url}"}}"#);
    let (first_url, first_server, _) = owned_recorded_server(vec![(
        200,
        first_body.into_bytes(),
        vec![("Content-Type", "application/json")],
    )])
    .await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": first_url,
                "method": "GET",
                "idempotency": {"name": "Authorization", "location": "header"},
                "response": {"records_path": "items"},
                "pagination": {
                    "type": "link",
                    "link_path": "next",
                    "max_pages": 2,
                    "allow_cross_origin": true
                }
            },
            "options": {"idempotency_key": "page-key-1"}
        }),
    )
    .await;
    first_server.await.unwrap();
    second_server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    let second_request = second_observed.lock().unwrap()[0].to_ascii_lowercase();
    assert!(
        !second_request.contains("authorization"),
        "a credential-shaped idempotency header must not cross the origin: {second_request}"
    );
}

#[tokio::test]
async fn a_remote_cancellation_registered_before_a_revocation_is_not_authenticated() {
    // The cancellation request is materialized when the job is registered, while
    // the poll is still on the owning origin. The result URL then moves to
    // another origin and revokes the authorization. The cancellation fires after
    // that, so it must reflect the revocation and not the state it was built
    // with.
    let owner = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let elsewhere = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner_url = format!("http://{}/", owner.local_addr().unwrap());
    let elsewhere_url = format!("http://{}/", elsewhere.local_addr().unwrap());

    let owner_requests = Arc::new(StdMutex::new(Vec::new()));
    let owner_server = serve_json(
        owner,
        vec![
            // Submission and poll, both on the owning origin.
            r#"{"id":"job-7","status":"pending","poll":"/jobs/job-7"}"#.to_owned(),
            format!(r#"{{"status":"completed","result":"{elsewhere_url}"}}"#),
            // The cancellation that cancelling the execution triggers.
            r#"{"cancelled":true}"#.to_owned(),
        ],
        owner_requests.clone(),
    );
    // Accepts the result request and never answers, so the execution is still
    // in flight — past the revocation — when it is cancelled. The channel makes
    // that ordering explicit: sleeping instead would make the test depend on
    // the machine being fast enough, and a slow runner would cancel before the
    // revocation, where an authenticated cancellation is in fact correct.
    let (revoked, revocation) = tokio::sync::oneshot::channel();
    let stalled = tokio::spawn(async move {
        let (stream, _) = elsewhere.accept().await.unwrap();
        let _ = revoked.send(());
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(stream);
    });

    let request: ExecutionRequest = serde_json::from_value(json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": owner_url,
            "method": "GET",
            "auth": {"type": "bearer", "token": "super-secret"},
            "polling": {
                "id_path": "id",
                "url_path": "poll",
                "status_path": "status",
                "result_url_path": "result",
                "interval_ms": 0,
                "max_attempts": 3,
                "allow_cross_origin": true,
                "cancel": {"url_template": "/jobs/job-7/cancel", "method": "POST"}
            }
        }
    }))
    .unwrap();

    let engine = Arc::new(local_engine());
    let cancellation = CancellationToken::new();
    let execution_engine = Arc::clone(&engine);
    let execution_cancellation = cancellation.clone();
    let execution = tokio::spawn(async move {
        execution_engine
            .execute_with_control(request, ExecutionControl::new(execution_cancellation))
            .await
    });
    timeout(Duration::from_secs(5), revocation)
        .await
        .expect("the result request never reached the other origin")
        .expect("the stalled server ended early");
    cancellation.cancel();

    let result = timeout(Duration::from_secs(5), execution)
        .await
        .expect("execution did not observe cancellation")
        .unwrap();
    timeout(Duration::from_secs(5), owner_server)
        .await
        .expect("the remote cancellation was not sent")
        .unwrap();
    stalled.abort();

    assert_eq!(result.status, plenora_rest_core::ExecutionStatus::Failed);
    let owner_requests = owner_requests.lock().unwrap();
    assert!(
        owner_requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer super-secret"),
        "the submission owns the credentials: {}",
        owner_requests[0]
    );
    let cancellation_request = owner_requests
        .iter()
        .find(|request| request.starts_with("POST "))
        .expect("the cancellation request should have reached the owning origin")
        .to_ascii_lowercase();
    assert!(
        !cancellation_request.contains("authorization")
            && !cancellation_request.contains("super-secret"),
        "a cancellation sent after the revocation must not be authenticated: {cancellation_request}"
    );
}

#[tokio::test]
async fn the_cache_refuses_to_run_alongside_the_cookie_store() {
    // A cached entry belongs to the session that produced it, but the jar
    // changes while the operation runs: a retry or a redirect can pick up a new
    // session after the key was computed, and cookies expire on their own. The
    // engine therefore refuses the combination outright.
    //
    // OAuth is configured on purpose: the refusal has to happen before anything
    // reaches the network, so the token endpoint must never be contacted. With
    // the check left only in the cache layer — which runs after authentication
    // — this listener would see a connection.
    let token_endpoint = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/token", token_endpoint.local_addr().unwrap());
    let contacted = Arc::new(StdMutex::new(false));
    let watcher_flag = contacted.clone();
    let watcher = tokio::spawn(async move {
        if timeout(Duration::from_secs(2), token_endpoint.accept())
            .await
            .is_ok()
        {
            *watcher_flag.lock().unwrap() = true;
        }
    });

    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        ..EngineConfig::default()
    });
    let cookie_session = engine.open_cookie_session().await.unwrap();
    let result = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": "http://127.0.0.1:9/",
                "method": "GET",
                "auth": {
                    "type": "oauth2_client_credentials",
                    "token_url": token_url,
                    "client_id": "id",
                    "client_secret": "secret"
                },
                "cookies": {"session": cookie_session},
                "cache": {"enabled": true, "fresh_for_ms": 600_000, "allow_authenticated": true}
            }
        }),
    )
    .await;
    watcher.await.unwrap();

    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["errors"][0]["code"], "POLICY_VIOLATION");
    assert!(
        !*contacted.lock().unwrap(),
        "a refused request must not authenticate first"
    );
}

#[tokio::test]
async fn many_cookie_sessions_do_not_wedge_the_engine() {
    // A pooled client counts as a user of its jar, so with the pool as large as
    // the jar registry every jar can look busy at once. Releasing the clients of
    // the oldest jar is what keeps new sessions possible; without it the engine
    // would refuse every later session for the rest of its life.
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        max_pooled_origins: 300,
        ..EngineConfig::default()
    });
    // Well past the 256 jar bound, each with its own session.
    for index in 0..300 {
        let cookie_session = engine.open_cookie_session().await.unwrap();
        let (url, server, _) = recorded_server(vec![(
            200,
            r#"{"ok":true}"#,
            vec![("Set-Cookie", "sid=x; Path=/")],
        )])
        .await;
        let result = execute(
            &engine,
            json!({
                "schema_version": 1,
                "operation": "test",
                "connection": {
                    "url": url,
                    "method": "GET",
                    "cookies": {"session": cookie_session}
                }
            }),
        )
        .await;
        server.await.unwrap();
        assert_eq!(
            result["status"], "success",
            "session {index} must still be admitted: {result}"
        );
    }
}

#[tokio::test]
async fn a_busy_oldest_jar_does_not_block_new_sessions() {
    // The oldest jar is the natural eviction candidate, but here it has a
    // request in flight. A new session must still be admitted by freeing one of
    // the others rather than giving up on the first candidate.
    let engine = Arc::new(Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        max_pooled_origins: 300,
        ..EngineConfig::default()
    }));
    let session = |url: &str, jar: &CookieSession| {
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                // Far longer than the loop below can take, so the stalled
                // request cannot finish on its own and hand the test a pass it
                // did not earn.
                "request": {"timeout_ms": 600_000},
                "cookies": {"session": jar}
            }
        })
    };

    // The oldest jar, left waiting on a server that never answers.
    let stalled = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalled_url = format!("http://{}/", stalled.local_addr().unwrap());
    let (accepted, arrival) = tokio::sync::oneshot::channel();
    let stalled_server = tokio::spawn(async move {
        let (stream, _) = stalled.accept().await.unwrap();
        let _ = accepted.send(());
        // Held until the test aborts this task, so the request stays in flight
        // however long the loop below takes.
        std::future::pending::<()>().await;
        drop(stream);
    });
    let holder_engine = Arc::clone(&engine);
    let oldest = engine.open_cookie_session().await.unwrap();
    let holder =
        tokio::spawn(async move { execute(&holder_engine, session(&stalled_url, &oldest)).await });
    timeout(Duration::from_secs(5), arrival)
        .await
        .expect("the long request never reached its server")
        .expect("the stalled server ended early");

    // Fill the registry behind it, then ask for one more.
    for index in 0..MAX_COOKIE_JARS_IN_TEST {
        assert!(
            !holder.is_finished(),
            "the oldest jar must still be busy at session {index}, \
             or the eviction was never forced past its first candidate"
        );
        let (url, server, _) = recorded_server(vec![(200, r#"{"ok":true}"#, vec![])]).await;
        let fresh = engine.open_cookie_session().await.unwrap();
        let result = execute(&engine, session(&url, &fresh)).await;
        assert_eq!(
            result["status"], "success",
            "session {index} must be admitted while the oldest jar is busy: {result}"
        );
        server.await.unwrap();
    }

    holder.abort();
    stalled_server.abort();
}

/// One past the engine's session bound, so the loops force an eviction.
const MAX_COOKIE_JARS_IN_TEST: usize = 257;

#[tokio::test]
async fn a_jar_reserved_by_a_running_request_is_not_evicted() {
    // Evicting a jar that a request is still using would split one session in
    // two: the running request stores its cookies in the jar it is holding,
    // while the next request naming the same session is handed a fresh one.
    // The registry is driven well past its bound while the first request is
    // deliberately stuck, and the session has to survive it.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let held_url = format!("http://{}/", listener.local_addr().unwrap());
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let server_observed = Arc::clone(&observed);
    let server = tokio::spawn(async move {
        let response = |set_cookie: bool| {
            let body = r#"{"ok":true}"#;
            let cookie = if set_cookie {
                "Set-Cookie: sid=held; Path=/\r\n"
            } else {
                ""
            };
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        server_observed.lock().unwrap().push(request);
        let _ = arrived.send(());
        // Nothing is written until the test says so, so this request stays in
        // flight — and its jar reserved — while the registry fills behind it.
        released.await.unwrap();
        stream.write_all(response(true).as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();

        let (mut stream, _) = timeout(Duration::from_secs(30), listener.accept())
            .await
            .expect("expected the follow-up request")
            .unwrap();
        let request = read_request(&mut stream).await;
        server_observed.lock().unwrap().push(request);
        stream.write_all(response(false).as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    let engine = Arc::new(Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        max_pooled_origins: 300,
        ..EngineConfig::default()
    }));
    let session = |url: &str, jar: &CookieSession| {
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                // Far longer than the loop below can take, so the stuck request
                // cannot time out and release its jar on its own.
                "request": {"timeout_ms": 600_000},
                "cookies": {"session": jar}
            }
        })
    };

    let held_engine = Arc::clone(&engine);
    let held_target = held_url.clone();
    let held_session = engine.open_cookie_session().await.unwrap();
    let held_handle = held_session.clone();
    let held =
        tokio::spawn(
            async move { execute(&held_engine, session(&held_target, &held_handle)).await },
        );
    timeout(Duration::from_secs(5), arrival)
        .await
        .expect("the held request never reached its server")
        .expect("the server ended early");

    for index in 0..MAX_COOKIE_JARS_IN_TEST {
        assert!(
            !held.is_finished(),
            "the held jar must still be reserved at session {index}, \
             or the eviction was never put to the test"
        );
        let (url, filler, _) = recorded_server(vec![(200, r#"{"ok":true}"#, vec![])]).await;
        let fresh = engine.open_cookie_session().await.unwrap();
        let result = execute(&engine, session(&url, &fresh)).await;
        assert_eq!(
            result["status"], "success",
            "session {index} must be admitted: {result}"
        );
        filler.await.unwrap();
    }

    release.send(()).unwrap();
    let first = held.await.unwrap();
    assert_eq!(first["status"], "success", "{first}");
    let second = execute(&engine, session(&held_url, &held_session)).await;
    assert_eq!(second["status"], "success", "{second}");
    server.await.unwrap();

    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 2);
    assert!(
        observed[1]
            .to_ascii_lowercase()
            .contains("cookie: sid=held"),
        "the reserved jar must survive the eviction pressure: {}",
        observed[1]
    );
}

#[tokio::test]
async fn oversized_set_cookie_headers_are_dropped_at_the_documented_bound() {
    // 8192 bytes is accepted, 8193 is not: a remote service must not be able to
    // push bulk data into engine-held state one header at a time.
    let small = format!(
        "small={}; Path=/",
        "a".repeat(8192 - "small=; Path=/".len())
    );
    let large = format!(
        "large={}; Path=/",
        "a".repeat(8193 - "large=; Path=/".len())
    );
    assert_eq!(small.len(), 8192);
    assert_eq!(large.len(), 8193);
    let (url, server, observed) = owned_recorded_server(vec![
        (
            200,
            b"{}".to_vec(),
            vec![
                ("Set-Cookie", Box::leak(small.into_boxed_str())),
                ("Set-Cookie", Box::leak(large.into_boxed_str())),
            ],
        ),
        (200, b"{}".to_vec(), Vec::new()),
    ])
    .await;
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        ..EngineConfig::default()
    });
    let cookie_session = engine.open_cookie_session().await.unwrap();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "cookies": {"session": cookie_session}
        }
    });

    execute(&engine, request.clone()).await;
    let second = execute(&engine, request).await;
    server.await.unwrap();

    assert_eq!(second["status"], "success", "{second}");
    let sent = observed.lock().unwrap()[1].to_ascii_lowercase();
    assert!(
        sent.contains("small="),
        "a cookie at the bound must be kept: {}",
        &sent[..sent.len().min(200)]
    );
    assert!(
        !sent.contains("large="),
        "a cookie past the bound must be dropped: {}",
        &sent[..sent.len().min(200)]
    );
}

#[tokio::test]
async fn a_cookie_jar_outlives_the_clients_that_use_it() {
    // The jar belongs to the engine, not to a pooled client. With pooling off
    // entirely, every request provably builds a fresh client, so the session can
    // only survive if the jar outlives it.
    let (first_url, first_server, first_observed) = recorded_server(vec![
        (200, r#"{"step":1}"#, vec![("Set-Cookie", "sid=a; Path=/")]),
        (200, r#"{"step":3}"#, vec![]),
    ])
    .await;
    let (second_url, second_server, _) =
        recorded_server(vec![(200, r#"{"step":2}"#, vec![])]).await;
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        max_pooled_origins: 0,
        ..EngineConfig::default()
    });
    let cookie_session = engine.open_cookie_session().await.unwrap();
    let with_jar = |url: &str| {
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "cookies": {"session": cookie_session}
            }
        })
    };

    execute(&engine, with_jar(&first_url)).await;
    execute(&engine, with_jar(&second_url)).await;
    let third = execute(&engine, with_jar(&first_url)).await;
    first_server.await.unwrap();
    second_server.await.unwrap();

    assert_eq!(third["status"], "success", "{third}");
    let first_observed = first_observed.lock().unwrap();
    assert_eq!(first_observed.len(), 2);
    assert!(
        first_observed[1]
            .to_ascii_lowercase()
            .contains("cookie: sid=a"),
        "the session must outlive the client that first sent it: {}",
        first_observed[1]
    );
}

#[tokio::test]
async fn a_cached_response_is_not_shared_across_redirect_policies() {
    // The first request is allowed to follow the redirect and caches what the
    // target answered. The second forbids redirects, so it must see the 3xx
    // rather than the followed result.
    let (url, server, _) = recorded_server(vec![
        (
            302,
            "",
            vec![("Location", "/target"), ("Cache-Control", "no-store")],
        ),
        (
            200,
            r#"{"followed":true}"#,
            vec![("Cache-Control", "max-age=600")],
        ),
        (
            302,
            "",
            vec![("Location", "/target"), ("Cache-Control", "no-store")],
        ),
    ])
    .await;
    let engine = local_engine();

    let followed = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "request": {"allow_redirects": true, "max_redirects": 3},
                "cache": {"enabled": true, "fresh_for_ms": 600_000},
                "success_statuses": [302]
            }
        }),
    )
    .await;
    let direct = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "request": {"allow_redirects": false},
                "cache": {"enabled": true, "fresh_for_ms": 600_000},
                "success_statuses": [302]
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(followed["output"]["value"]["followed"], true);
    assert_eq!(
        direct["metrics"]["cache_hits"], 0,
        "a request that forbids redirects must not be served a followed response: {direct}"
    );
    assert_eq!(direct["output"]["value"], Value::Null);
}

#[tokio::test]
async fn wildcard_response_capture_hides_vendor_credential_headers() {
    let (url, server, _) = recorded_server(vec![(
        200,
        r#"{"ok":true}"#,
        vec![
            ("ETag", "\"v1\""),
            ("X-Auth-Token", "vendor-secret"),
            ("X-Amz-Security-Token", "aws-secret"),
            ("X-RateLimit-Remaining", "42"),
        ],
    )])
    .await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {"url": url, "method": "GET"},
            "options": {"capture_response_metadata": true, "response_headers": ["*"]}
        }),
    )
    .await;
    server.await.unwrap();

    let headers = &result["responses"][0]["headers"];
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(headers["x-ratelimit-remaining"], "42");
    assert!(headers.get("x-auth-token").is_none());
    assert!(headers.get("x-amz-security-token").is_none());
}

#[tokio::test]
async fn cached_responses_report_a_contract_valid_attempt_count() {
    let (url, server, _) = recorded_server(vec![(
        200,
        r#"{"ok":true}"#,
        vec![("Cache-Control", "max-age=60")],
    )])
    .await;
    let engine = local_engine();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "cache": {"enabled": true, "fresh_for_ms": 60_000}
        },
        "options": {"capture_response_metadata": true}
    });

    let first = execute(&engine, request.clone()).await;
    let second = execute(&engine, request).await;
    server.await.unwrap();

    assert_eq!(first["status"], "success");
    assert_eq!(second["status"], "success");
    assert_eq!(second["metrics"]["cache_hits"], 1);
    // The v1 execution result schema declares `attempts` with `minimum: 1`.
    for result in [&first, &second] {
        assert!(
            result["responses"][0]["attempts"].as_u64().unwrap() >= 1,
            "attempts must satisfy the published contract: {result}"
        );
    }
}

#[tokio::test]
async fn numeric_transforms_preserve_integers_beyond_float_precision() {
    let identifier = 9_007_199_254_740_993_i64;
    let body = format!(r#"{{"id":{identifier},"count":7}}"#);
    let (url, server, _) = owned_recorded_server(vec![(200, body.into_bytes(), vec![])]).await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {
                    "output_mapping": [
                        {"path": "id", "column": "id"},
                        {"path": "count", "column": "count"}
                    ],
                    "transforms": [
                        {"source": "id", "column": "id", "operation": "add", "value": 1},
                        {"source": "count", "column": "halved", "operation": "divide", "value": 2}
                    ]
                }
            }
        }),
    )
    .await;
    server.await.unwrap();

    let record = &result["output"]["records"][0];
    assert_eq!(record["id"], json!(identifier + 1));
    // A non-exact integer division still yields the floating point result.
    assert_eq!(record["halved"], json!(3.5));
}

#[tokio::test]
async fn numeric_transforms_cover_unsigned_overflow_and_rounding_edges() {
    let body = format!(r#"{{"big":{},"small":{},"exact":9}}"#, u64::MAX, i64::MIN);
    let (url, server, _) = owned_recorded_server(vec![(200, body.into_bytes(), vec![])]).await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {
                    "output_mapping": [
                        {"path": "big", "column": "big"},
                        {"path": "small", "column": "small"},
                        {"path": "exact", "column": "exact"}
                    ],
                    "transforms": [
                        // An unsigned value above i64::MAX stays exact.
                        {"source": "big", "column": "rounded", "operation": "round", "value": 4},
                        {"source": "big", "column": "minus_one", "operation": "subtract", "value": 1},
                        // i64::MIN / -1 has no i64 representation but fits u64.
                        {"source": "small", "column": "negated", "operation": "divide", "value": -1},
                        {"source": "exact", "column": "thirds", "operation": "divide", "value": 3}
                    ]
                }
            }
        }),
    )
    .await;
    server.await.unwrap();

    let record = &result["output"]["records"][0];
    assert_eq!(record["rounded"], json!(u64::MAX));
    assert_eq!(record["minus_one"], json!(u64::MAX - 1));
    assert_eq!(record["negated"], json!(i64::MIN.unsigned_abs()));
    assert_eq!(record["thirds"], json!(3));
}

#[tokio::test]
async fn xml_responses_decode_entities_and_normalize_attribute_whitespace() {
    // `&amp;` and friends are mandatory XML escaping: a parser that rejects
    // them cannot read ordinary payloads. A literal tab or newline inside an
    // attribute is normalized to a space, while a character reference is not,
    // which is what XML 1.0 attribute-value normalization prescribes.
    let body = concat!(
        "<?xml version=\"1.0\"?>",
        "<item id=\"a&amp;b\" note=\"line
break	and tab\" code=\"&#65;&#x42;\">",
        "<name>Ada&lt;Lovelace&gt;</name>",
        "</item>"
    );
    let (url, server, _) =
        recorded_server(vec![(200, body, vec![("Content-Type", "application/xml")])]).await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {"format": "xml"}
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    let item = &result["output"]["value"]["item"];
    assert_eq!(item["@id"], "a&b");
    assert_eq!(item["@note"], "line break and tab");
    assert_eq!(item["@code"], "AB");
    assert_eq!(item["name"], "Ada<Lovelace>");
}

#[tokio::test]
async fn xml_responses_still_refuse_dtds_and_unknown_entities() {
    for body in [
        "<!DOCTYPE item [<!ENTITY x \"y\">]><item>&x;</item>",
        "<item>&unknown;</item>",
    ] {
        let (url, server, _) =
            recorded_server(vec![(200, body, vec![("Content-Type", "application/xml")])]).await;
        let result = execute(
            &local_engine(),
            json!({
                "schema_version": 1,
                "operation": "test",
                "connection": {
                    "url": url,
                    "method": "GET",
                    "response": {"format": "xml"}
                }
            }),
        )
        .await;
        server.await.unwrap();
        assert_eq!(result["status"], "failed", "{body} must be refused");
        assert_eq!(result["errors"][0]["code"], "INVALID_RESPONSE");
    }
}

#[tokio::test]
async fn download_streams_beyond_the_in_memory_limit_and_replaces_on_success() {
    let directory = transfer_directory("download");
    let destination = directory.join("artifact.bin");
    fs::write(&destination, b"old").await.unwrap();
    let body = vec![b'x'; 256 * 1024];
    let expected_sha256 = sha256(&body);
    let (url, server) = binary_server(
        200,
        body.clone(),
        vec![("Content-Type", "application/octet-stream")],
    )
    .await;
    let engine = transfer_engine(&directory, 1024 * 1024, 8);
    let result = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": url, "method": "GET"},
            "input": {
                "file": {
                    "path": "artifact.bin",
                    "overwrite": true,
                    "expected_sha256": expected_sha256
                }
            },
            "options": {
                "capture_response_metadata": true,
                "response_headers": ["content-type"]
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["type"], "file");
    assert_eq!(result["output"]["direction"], "download");
    assert_eq!(result["output"]["bytes_transferred"], body.len());
    assert_eq!(result["output"]["checksum"]["value"], sha256(&body));
    assert_eq!(result["metrics"]["bytes_downloaded"], body.len());
    assert_eq!(
        result["responses"][0]["headers"]["content-type"],
        "application/octet-stream"
    );
    assert_eq!(fs::read(&destination).await.unwrap(), body);
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn resumable_download_uses_range_and_if_range_after_an_interruption() {
    let directory = transfer_directory("resume");
    let destination = directory.join("resumed.bin");
    let body = vec![b'r'; 128 * 1024];
    let split = 32 * 1024;
    let content_range = format!("bytes {split}-{}/{}", body.len() - 1, body.len());
    let (url, server, observed) = interrupted_binary_server(
        body.len(),
        body[..split].to_vec(),
        vec![("ETag".to_owned(), "\"v1\"".to_owned())],
        206,
        body[split..].to_vec(),
        vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            ("Content-Range".to_owned(), content_range),
        ],
    )
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024 * 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 2, "backoff_base_ms": 0}
            },
            "input": {
                "file": {
                    "path": "resumed.bin",
                    "resume": true,
                    "expected_sha256": sha256(&body)
                }
            },
            "options": {
                "capture_response_metadata": true,
                "response_headers": ["etag", "content-range"]
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["bytes_transferred"], body.len());
    assert_eq!(result["metrics"]["bytes_downloaded"], body.len());
    assert_eq!(result["metrics"]["requests"], 2);
    assert_eq!(result["metrics"]["retries"], 1);
    assert_eq!(result["responses"][0]["status"], 206);
    assert_eq!(fs::read(&destination).await.unwrap(), body);
    {
        let observed = observed.lock().unwrap();
        let first = observed[0].to_ascii_lowercase();
        let second = observed[1].to_ascii_lowercase();
        assert!(first.contains("accept-encoding: identity"));
        assert!(second.contains(&format!("range: bytes={split}-")));
        assert!(second.contains("if-range: \"v1\""));
        assert!(second.contains("accept-encoding: identity"));
    }
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn resumable_download_restarts_when_the_server_returns_a_full_response() {
    let directory = transfer_directory("resume-restart");
    let split = 8 * 1024;
    let old_body = vec![b'o'; 64 * 1024];
    let new_body = vec![b'n'; 48 * 1024];
    let (url, server, observed) = interrupted_binary_server(
        old_body.len(),
        old_body[..split].to_vec(),
        vec![("ETag".to_owned(), "\"old\"".to_owned())],
        200,
        new_body.clone(),
        vec![("ETag".to_owned(), "\"new\"".to_owned())],
    )
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024 * 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 2, "backoff_base_ms": 0}
            },
            "input": {
                "file": {
                    "path": "restarted.bin",
                    "resume": true,
                    "expected_sha256": sha256(&new_body)
                }
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["bytes_transferred"], new_body.len());
    assert_eq!(
        result["metrics"]["bytes_downloaded"],
        split + new_body.len()
    );
    assert_eq!(
        fs::read(directory.join("restarted.bin")).await.unwrap(),
        new_body
    );
    let second = observed.lock().unwrap()[1].to_ascii_lowercase();
    assert!(second.contains(&format!("range: bytes={split}-")));
    assert!(second.contains("if-range: \"old\""));
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn resumable_download_restarts_without_a_strong_etag() {
    let directory = transfer_directory("resume-weak-etag");
    let split = 4 * 1024;
    let body = vec![b'w'; 32 * 1024];
    let (url, server, observed) = interrupted_binary_server(
        body.len(),
        body[..split].to_vec(),
        vec![("ETag".to_owned(), "W/\"v1\"".to_owned())],
        200,
        body.clone(),
        vec![("ETag".to_owned(), "W/\"v1\"".to_owned())],
    )
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024 * 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 2, "backoff_base_ms": 0}
            },
            "input": {"file": {"path": "weak.bin", "resume": true}}
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["metrics"]["bytes_downloaded"], split + body.len());
    assert_eq!(fs::read(directory.join("weak.bin")).await.unwrap(), body);
    let second = observed.lock().unwrap()[1].to_ascii_lowercase();
    assert!(!second.contains("\r\nrange:"));
    assert!(!second.contains("\r\nif-range:"));
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn resumable_download_rejects_inconsistent_partial_responses() {
    let directory = transfer_directory("resume-invalid-range");
    let body = vec![b'x'; 32 * 1024];
    let split = 4 * 1024;
    let wrong_start = split + 1;
    let content_range = format!("bytes {wrong_start}-{}/{}", body.len() - 1, body.len());
    let (url, server, _) = interrupted_binary_server(
        body.len(),
        body[..split].to_vec(),
        vec![("ETag".to_owned(), "\"v1\"".to_owned())],
        206,
        body[wrong_start..].to_vec(),
        vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            ("Content-Range".to_owned(), content_range),
        ],
    )
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024 * 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 2, "backoff_base_ms": 0}
            },
            "input": {"file": {"path": "invalid.bin", "resume": true}}
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "INVALID_RESPONSE");
    assert!(!fs::try_exists(directory.join("invalid.bin")).await.unwrap());
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn resumable_download_rejects_a_changed_etag_on_partial_content() {
    let directory = transfer_directory("resume-changed-etag");
    let body = vec![b'e'; 32 * 1024];
    let split = 4 * 1024;
    let content_range = format!("bytes {split}-{}/{}", body.len() - 1, body.len());
    let (url, server, _) = interrupted_binary_server(
        body.len(),
        body[..split].to_vec(),
        vec![("ETag".to_owned(), "\"v1\"".to_owned())],
        206,
        body[split..].to_vec(),
        vec![
            ("ETag".to_owned(), "\"v2\"".to_owned()),
            ("Content-Range".to_owned(), content_range),
        ],
    )
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024 * 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 2, "backoff_base_ms": 0}
            },
            "input": {"file": {"path": "changed.bin", "resume": true}}
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "INVALID_RESPONSE");
    assert!(!fs::try_exists(directory.join("changed.bin")).await.unwrap());
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn resumable_download_rejects_a_changed_total_length() {
    let directory = transfer_directory("resume-changed-total");
    let body = vec![b't'; 32 * 1024];
    let split = 4 * 1024;
    let changed_total = body.len() + 1;
    let content_range = format!("bytes {split}-{}/{}", changed_total - 1, changed_total);
    let mut remaining = body[split..].to_vec();
    remaining.push(b't');
    let (url, server, _) = interrupted_binary_server(
        body.len(),
        body[..split].to_vec(),
        vec![("ETag".to_owned(), "\"v1\"".to_owned())],
        206,
        remaining,
        vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            ("Content-Range".to_owned(), content_range),
        ],
    )
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024 * 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 2, "backoff_base_ms": 0}
            },
            "input": {"file": {"path": "changed-total.bin", "resume": true}}
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "INVALID_RESPONSE");
    assert!(
        !fs::try_exists(directory.join("changed-total.bin"))
            .await
            .unwrap()
    );
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn partial_responses_are_not_promoted_without_a_complete_representation() {
    let directory = transfer_directory("partial-response");
    let (url, server) = binary_server(
        206,
        b"tail".to_vec(),
        vec![("Content-Range", "bytes 5-8/9"), ("ETag", "\"v1\"")],
    )
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": url, "method": "GET"},
            "input": {"file": {"path": "partial.bin"}}
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "INVALID_RESPONSE");
    assert!(!fs::try_exists(directory.join("partial.bin")).await.unwrap());
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn managed_resume_rejects_non_get_and_user_managed_range_headers() {
    let directory = transfer_directory("resume-contract");
    let engine = transfer_engine(&directory, 1024, 1024);
    let non_get = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": "http://127.0.0.1:9/file", "method": "POST"},
            "input": {"file": {"path": "post.bin", "resume": true}}
        }),
    )
    .await;
    let manual_range = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": "http://127.0.0.1:9/file",
                "method": "GET",
                "headers": {"Range": "bytes=10-"}
            },
            "input": {"file": {"path": "range.bin", "resume": true}}
        }),
    )
    .await;

    assert_eq!(non_get["errors"][0]["code"], "INVALID_INPUT");
    assert_eq!(manual_range["errors"][0]["code"], "INVALID_INPUT");
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn polling_can_finish_with_a_streamed_download() {
    let directory = transfer_directory("polled-download");
    let destination = directory.join("async.bin");
    let body = vec![b'r'; 128 * 1024];
    let expected_sha256 = sha256(&body);
    let (base_url, server, observed) = owned_recorded_server(vec![
        (202, Vec::new(), vec![("X-Job-Id", "export/1")]),
        (200, br#"{"status":"running"}"#.to_vec(), vec![]),
        (200, br#"{"status":"completed"}"#.to_vec(), vec![]),
        (
            200,
            body.clone(),
            vec![("Content-Type", "application/octet-stream")],
        ),
    ])
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024 * 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": format!("{base_url}exports"),
                "method": "POST",
                "polling": {
                    "url_template": "{base}/jobs/{job_id}",
                    "id_header": "X-Job-Id",
                    "location_header": null,
                    "status_path": "status",
                    "result_url_template": "{base}/artifacts/{job_id}",
                    "interval_ms": 0,
                    "max_attempts": 3
                }
            },
            "input": {
                "file": {
                    "path": "async.bin",
                    "resume": true,
                    "expected_sha256": expected_sha256
                }
            },
            "options": {
                "capture_response_metadata": true,
                "response_headers": ["content-type"]
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["type"], "file");
    assert_eq!(result["output"]["direction"], "download");
    assert_eq!(result["output"]["bytes_transferred"], body.len());
    assert_eq!(result["output"]["checksum"]["value"], sha256(&body));
    assert_eq!(result["metrics"]["requests"], 4);
    assert_eq!(result["metrics"]["poll_requests"], 3);
    assert_eq!(result["metrics"]["bytes_downloaded"], body.len());
    assert_eq!(
        result["responses"][0]["headers"]["content-type"],
        "application/octet-stream"
    );
    assert_eq!(fs::read(&destination).await.unwrap(), body);
    {
        let observed = observed.lock().unwrap();
        assert!(observed[0].starts_with("POST /exports "));
        assert!(observed[1].starts_with("GET /jobs/export%2F1 "));
        assert!(observed[2].starts_with("GET /jobs/export%2F1 "));
        assert!(observed[3].starts_with("GET /artifacts/export%2F1 "));
    }
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn polling_blocks_cross_origin_download_results_before_creating_output() {
    let directory = transfer_directory("polled-download-origin");
    let (base_url, server, _) = recorded_server(vec![
        (202, "", vec![("Location", "/jobs/1")]),
        (
            200,
            r#"{"status":"completed","result_url":"http://127.0.0.1:9/artifact"}"#,
            vec![],
        ),
    ])
    .await;
    let result = execute(
        &transfer_engine(&directory, 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": format!("{base_url}exports"),
                "method": "POST",
                "polling": {
                    "status_path": "status",
                    "result_url_path": "result_url",
                    "interval_ms": 0,
                    "max_attempts": 2
                }
            },
            "input": {"file": {"path": "blocked.bin"}}
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "UNSAFE_ADDRESS");
    assert_eq!(result["metrics"]["requests"], 2);
    assert_eq!(result["metrics"]["poll_requests"], 1);
    assert!(!fs::try_exists(directory.join("blocked.bin")).await.unwrap());
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn download_limit_and_checksum_failures_leave_no_output() {
    let directory = transfer_directory("download-failures");
    let body = vec![b'z'; 64];
    let (limit_url, limit_server) = binary_server(200, body.clone(), vec![]).await;
    let engine = transfer_engine(&directory, 1024, 8);
    let limited = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": limit_url, "method": "GET"},
            "input": {"file": {"path": "limited.bin", "max_bytes": 16}}
        }),
    )
    .await;
    limit_server.await.unwrap();
    assert_eq!(limited["errors"][0]["code"], "FILE_TOO_LARGE");
    assert!(!fs::try_exists(directory.join("limited.bin")).await.unwrap());

    let (checksum_url, checksum_server) = binary_server(200, body, vec![]).await;
    let checksum = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": checksum_url, "method": "GET"},
            "input": {
                "file": {
                    "path": "checksum.bin",
                    "expected_sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            }
        }),
    )
    .await;
    checksum_server.await.unwrap();
    assert_eq!(checksum["errors"][0]["code"], "CHECKSUM_MISMATCH");
    assert!(
        !fs::try_exists(directory.join("checksum.bin"))
            .await
            .unwrap()
    );
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn raw_upload_streams_beyond_the_in_memory_request_limit() {
    let directory = transfer_directory("raw-upload");
    let source = directory.join("payload.bin");
    let body = vec![b'u'; 128 * 1024];
    fs::write(&source, &body).await.unwrap();
    let (url, server, observed) =
        recorded_server(vec![(200, r#"{"uploaded":true}"#, vec![])]).await;
    let result = execute(
        &transfer_engine(&directory, 1024 * 1024, 64),
        json!({
            "schema_version": 1,
            "operation": "upload",
            "connection": {
                "url": url,
                "method": "PUT",
                "request": {"body_type": "raw"}
            },
            "input": {
                "file": {
                    "path": "payload.bin",
                    "content_type": "application/octet-stream"
                }
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["direction"], "upload");
    assert_eq!(result["output"]["bytes_transferred"], body.len());
    assert_eq!(result["output"]["checksum"]["value"], sha256(&body));
    assert_eq!(result["output"]["response"], json!({"uploaded": true}));
    assert_eq!(result["metrics"]["bytes_uploaded"], body.len());
    let request = observed.lock().unwrap()[0].clone();
    assert!(request.starts_with("PUT / "));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: application/octet-stream")
    );
    assert!(request.contains(&format!("content-length: {}", body.len())));
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn multipart_upload_streams_the_file_and_keeps_regular_fields() {
    let directory = transfer_directory("multipart-upload");
    let source = directory.join("payload.txt");
    fs::write(&source, b"streamed-file-content").await.unwrap();
    let (url, server, observed) =
        recorded_server(vec![(200, r#"{"uploaded":true}"#, vec![])]).await;
    let result = execute(
        &transfer_engine(&directory, 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "upload",
            "connection": {
                "url": url,
                "method": "POST",
                "request": {"body_type": "multipart"}
            },
            "input": {
                "params": {"description": "document"},
                "file": {
                    "path": "payload.txt",
                    "field_name": "attachment",
                    "filename": "remote.txt",
                    "content_type": "text/plain"
                }
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    let request = observed.lock().unwrap()[0].clone();
    assert!(request.contains("description"));
    assert!(request.contains("document"));
    assert!(request.contains("attachment"));
    assert!(request.contains("remote.txt"));
    assert!(request.contains("streamed-file-content"));
    fs::remove_dir_all(directory).await.unwrap();
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
async fn idempotency_key_is_stable_across_retries_and_conflicts_fail_before_network() {
    let (url, server, observed) = recorded_server(vec![
        (503, r#"{"error":"busy"}"#, vec![]),
        (200, r#"{"ok":true}"#, vec![]),
    ])
    .await;
    let engine = local_engine();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "POST",
            "request": {"body_type": "json"},
            "retry": {
                "max_attempts": 2,
                "backoff_base_ms": 0,
                "retry_on_status": [503]
            }
        },
        "input": {"params": {"value": 1}},
        "options": {"idempotency_key": "job-request-42"}
    });

    let first = execute(&engine, request.clone()).await;
    server.await.unwrap();
    assert_eq!(first["status"], "success");
    assert_eq!(first["metrics"]["requests"], 2);

    let mut conflicting = request;
    conflicting["input"]["params"]["value"] = json!(2);
    let conflict = execute(&engine, conflicting).await;
    assert_eq!(conflict["status"], "failed");
    assert_eq!(conflict["errors"][0]["code"], "IDEMPOTENCY_CONFLICT");

    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 2);
    for request in observed.iter() {
        assert!(
            request
                .to_ascii_lowercase()
                .contains("idempotency-key: job-request-42")
        );
    }
}

#[tokio::test]
async fn idempotency_key_supports_query_and_body_locations() {
    let (url, server, observed) = recorded_server(vec![
        (200, r#"{"ok":true}"#, vec![]),
        (200, r#"{"ok":true}"#, vec![]),
    ])
    .await;
    let engine = local_engine();

    let query_result = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "idempotency": {"name": "request_id", "location": "query"}
            },
            "options": {"idempotency_key": "query-42"}
        }),
    )
    .await;
    let body_result = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "POST",
                "request": {"body_type": "json"},
                "idempotency": {"name": "request_id", "location": "body"}
            },
            "input": {"params": {"value": 1}},
            "options": {"idempotency_key": "body-42"}
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(query_result["status"], "success");
    assert_eq!(body_result["status"], "success");
    let observed = observed.lock().unwrap();
    assert!(observed[0].starts_with("GET /?request_id=query-42 "));
    assert!(observed[1].contains(r#""request_id":"body-42""#));
}

#[tokio::test]
async fn polling_resume_skips_submission_and_uses_the_existing_job() {
    let (base_url, server, observed) = recorded_server(vec![(
        200,
        r#"{"status":"completed","result":{"answer":42}}"#,
        vec![],
    )])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{base_url}submit"),
            "method": "POST",
            "polling": {
                "url_template": "{base}/jobs/{job_id}",
                "status_path": "status",
                "result_path": "result",
                "interval_ms": 0,
                "max_attempts": 1,
                "resume": {"job_id": "existing-42"}
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(result["output"]["value"], json!({"answer": 42}));
    assert_eq!(result["metrics"]["requests"], 1);
    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert!(
        observed[0].starts_with("GET /jobs/existing-42 "),
        "{observed:?}"
    );
}

#[tokio::test]
async fn polling_timeout_returns_recovery_and_can_cancel_the_remote_job() {
    let (base_url, server, observed) = recorded_server(vec![
        (
            202,
            r#"{"id":"job-42","status":"queued"}"#,
            vec![("Location", "/jobs/job-42")],
        ),
        (200, r#"{"status":"running"}"#, vec![]),
        (200, r#"{"cancelled":true}"#, vec![]),
    ])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{base_url}submit"),
            "method": "POST",
            "polling": {
                "status_path": "status",
                "interval_ms": 0,
                "max_attempts": 1,
                "cancel": {"on_poll_timeout": true}
            }
        }
    });

    let result = execute(&local_engine(), request).await;
    server.await.unwrap();

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "POLLING_TIMEOUT");
    assert_eq!(
        result["recoveries"][0]["contract"],
        "plenora-rest-async-job-recovery-v1"
    );
    assert_eq!(result["recoveries"][0]["job_id"], "job-42");
    assert_eq!(result["recoveries"][0]["cancel_requested"], true);
    assert_eq!(result["recoveries"][0]["cancel_accepted"], true);
    let observed = observed.lock().unwrap();
    assert!(observed[0].starts_with("POST /submit "));
    assert!(observed[1].starts_with("GET /jobs/job-42 "));
    assert!(observed[2].starts_with("DELETE /jobs/job-42 "));
}

#[tokio::test]
async fn invalid_polling_configuration_is_rejected_before_remote_submission() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": format!("http://{address}/submit"),
                "method": "POST",
                "polling": {
                    "status_path": "status",
                    "max_attempts": 0
                }
            }
        }),
    )
    .await;

    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "INVALID_INPUT");
    assert_eq!(result["errors"][0]["remote_effect"], "none");
    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "invalid polling configuration reached the remote server"
    );
}

#[tokio::test]
async fn local_cancellation_attempts_remote_cancellation_and_preserves_recovery() {
    let (base_url, server, observed) = recorded_server(vec![
        (
            202,
            r#"{"id":"job-99","status":"queued"}"#,
            vec![("Location", "/jobs/job-99")],
        ),
        (200, r#"{"cancelled":true}"#, vec![]),
    ])
    .await;
    let request: ExecutionRequest = serde_json::from_value(json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{base_url}submit"),
            "method": "POST",
            "polling": {
                "status_path": "status",
                "interval_ms": 5_000,
                "max_attempts": 2,
                "cancel": {}
            }
        }
    }))
    .unwrap();
    let engine = Arc::new(local_engine());
    let cancellation = CancellationToken::new();
    let execution_engine = Arc::clone(&engine);
    let execution_cancellation = cancellation.clone();
    let execution = tokio::spawn(async move {
        execution_engine
            .execute_with_control(request, ExecutionControl::new(execution_cancellation))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancellation.cancel();

    let result = timeout(Duration::from_secs(2), execution)
        .await
        .expect("execution did not observe cancellation")
        .unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .expect("remote cancellation request was not sent")
        .unwrap();

    assert_eq!(result.status, plenora_rest_core::ExecutionStatus::Failed);
    assert_eq!(result.errors[0].code, "CANCELLED");
    assert_eq!(result.recoveries[0].job_id, "job-99");
    assert!(result.recoveries[0].cancel_requested);
    assert_eq!(result.recoveries[0].cancel_accepted, Some(true));
    let observed = observed.lock().unwrap();
    assert!(observed[0].starts_with("POST /submit "));
    assert!(observed[1].starts_with("DELETE /jobs/job-99 "));
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
    assert_eq!(result["errors"][0]["code"], "APPLICATION_ERROR");
    assert_eq!(
        result["errors"][0]["message"],
        "Remote application reported failure"
    );
}

#[tokio::test]
async fn dangerous_transport_options_are_denied_by_default() {
    let permissive = Engine::new(EngineConfig {
        allow_cookie_store: true,
        ..EngineConfig::default()
    });
    let cookie_session = permissive.open_cookie_session().await.unwrap();
    assert!(Engine::default().open_cookie_session().await.is_err());
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
        json!({
            "url": "https://example.com",
            "method": "GET",
            "cookies": {"session": cookie_session}
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
        assert_eq!(result["errors"][0]["code"], "POLICY_VIOLATION");
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

#[tokio::test]
async fn cookie_jars_are_persistent_and_explicitly_authorized() {
    let (url, server, observed) = recorded_server(vec![
        (
            200,
            r#"{"authenticated":true}"#,
            vec![("Set-Cookie", "session=abc123; Path=/; HttpOnly")],
        ),
        (200, r#"{"authenticated":true}"#, vec![]),
    ])
    .await;
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        ..EngineConfig::default()
    });
    let cookie_session = engine.open_cookie_session().await.unwrap();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "cookies": {"session": cookie_session}
        }
    });

    assert_eq!(execute(&engine, request.clone()).await["status"], "success");
    assert_eq!(execute(&engine, request).await["status"], "success");
    server.await.unwrap();

    let observed = observed.lock().unwrap();
    assert!(!observed[0].to_ascii_lowercase().contains("cookie:"));
    assert!(
        observed[1]
            .to_ascii_lowercase()
            .contains("cookie: session=abc123")
    );
}

#[tokio::test]
async fn conditional_cache_revalidates_and_can_serve_fresh_entries() {
    let (url, server, observed) = recorded_server(vec![
        (200, r#"{"version":1}"#, vec![("ETag", "\"version-1\"")]),
        (304, "", vec![("ETag", "\"version-1\"")]),
    ])
    .await;
    let engine = local_engine();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "cache": {"enabled": true}
        }
    });

    let first = execute(&engine, request.clone()).await;
    let second = execute(&engine, request.clone()).await;
    server.await.unwrap();
    assert_eq!(first["output"]["value"], json!({"version": 1}));
    assert_eq!(second["output"]["value"], json!({"version": 1}));
    assert_eq!(second["metrics"]["requests"], 1);
    assert_eq!(second["metrics"]["cache_hits"], 1);
    assert_eq!(second["metrics"]["cache_revalidations"], 1);
    assert!(
        observed.lock().unwrap()[1]
            .to_ascii_lowercase()
            .contains("if-none-match: \"version-1\"")
    );

    let mut fresh = request;
    fresh["connection"]["cache"]["fresh_for_ms"] = json!(60_000);
    let cached = execute(&engine, fresh).await;
    assert_eq!(cached["status"], "success");
    assert_eq!(cached["metrics"]["requests"], 0);
    assert_eq!(cached["metrics"]["cache_hits"], 1);
}

#[tokio::test]
async fn circuit_breaker_opens_after_the_configured_failures() {
    let (url, server, observed) = recorded_server(vec![
        (503, r#"{"error":"down"}"#, vec![]),
        (503, r#"{"error":"down"}"#, vec![]),
    ])
    .await;
    let engine = local_engine();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "circuit_breaker": {
                "enabled": true,
                "failure_threshold": 2,
                "recovery_timeout_ms": 60_000
            }
        }
    });

    assert_eq!(
        execute(&engine, request.clone()).await["errors"][0]["code"],
        "HTTP_STATUS"
    );
    assert_eq!(
        execute(&engine, request.clone()).await["errors"][0]["code"],
        "HTTP_STATUS"
    );
    let rejected = execute(&engine, request).await;
    server.await.unwrap();

    assert_eq!(rejected["errors"][0]["code"], "CIRCUIT_OPEN");
    assert_eq!(observed.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn concurrent_enrichment_preserves_input_order() {
    let (base_url, server) = barrier_server(3).await;
    let request = json!({
        "schema_version": 1,
        "operation": "enrich",
        "connection": {
            "url": format!("{base_url}users/{{id}}"),
            "method": "GET",
            "parameters": [{
                "name": "id",
                "mode": "mapped",
                "source": "id",
                "required": true,
                "location": "path"
            }]
        },
        "input": {"records": [{"id": 1}, {"id": 2}, {"id": 3}]},
        "options": {
            "continue_on_error": true,
            "enrichment_concurrency": 3
        }
    });

    let result = timeout(Duration::from_secs(5), execute(&local_engine(), request))
        .await
        .expect("enrichment did not issue requests concurrently");
    server.await.unwrap();

    assert_eq!(result["status"], "success");
    assert_eq!(
        result["output"]["records"],
        json!([
            {"id": 1, "remote": 1},
            {"id": 2, "remote": 2},
            {"id": 3, "remote": 3}
        ])
    );
    assert_eq!(result["metrics"]["requests"], 3);
}

fn local_engine() -> Engine {
    Engine::new(EngineConfig {
        allow_private_networks: true,
        ..EngineConfig::default()
    })
}

fn transfer_engine(directory: &Path, max_file_bytes: u64, max_memory_bytes: usize) -> Engine {
    Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_file_transfers: true,
        file_root: Some(directory.to_string_lossy().into_owned()),
        max_file_transfer_bytes: max_file_bytes,
        max_request_bytes: max_memory_bytes,
        max_response_bytes: max_memory_bytes,
        ..EngineConfig::default()
    })
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
type OwnedTestResponse = (u16, Vec<u8>, Vec<(&'static str, &'static str)>);

async fn recorded_server(
    responses: Vec<TestResponse>,
) -> (String, JoinHandle<()>, Arc<StdMutex<Vec<String>>>) {
    owned_recorded_server(
        responses
            .into_iter()
            .map(|(status, body, headers)| (status, body.as_bytes().to_vec(), headers))
            .collect(),
    )
    .await
}

/// Answers `bodies` in order on an already bound listener, recording each raw
/// request. Used when the response bodies have to reference the listener's own
/// address, which is only known after binding.
fn serve_json(
    listener: TcpListener,
    bodies: Vec<String>,
    observed: Arc<StdMutex<Vec<String>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        for body in bodies {
            // Bounded: a regression that breaks the chain should fail the test
            // rather than leave it waiting for a request that never comes.
            let (mut stream, _) = timeout(Duration::from_secs(30), listener.accept())
                .await
                .expect("expected another request")
                .unwrap();
            let request = read_request(&mut stream).await;
            observed.lock().unwrap().push(request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    })
}

async fn owned_recorded_server(
    responses: Vec<OwnedTestResponse>,
) -> (String, JoinHandle<()>, Arc<StdMutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let task_observed = observed.clone();
    let task = tokio::spawn(async move {
        for (status, body, headers) in responses {
            // Bounded: a regression that stops the chain early should fail the
            // test rather than leave it waiting for a request that never comes.
            let (mut stream, _) = timeout(Duration::from_secs(30), listener.accept())
                .await
                .expect("expected another request")
                .unwrap();
            let request = read_request(&mut stream).await;
            task_observed.lock().unwrap().push(request);
            let reason = match status {
                200 => "OK",
                202 => "Accepted",
                503 => "Service Unavailable",
                _ => "Response",
            };
            let extra_headers: String = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let default_content_type = if headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            {
                ""
            } else {
                "Content-Type: application/json\r\n"
            };
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\n{default_content_type}{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
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

async fn interrupted_binary_server(
    first_declared_length: usize,
    first_body: Vec<u8>,
    first_headers: Vec<(String, String)>,
    second_status: u16,
    second_body: Vec<u8>,
    second_headers: Vec<(String, String)>,
) -> (String, JoinHandle<()>, Arc<StdMutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let task_observed = observed.clone();
    let task = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let request = read_request(&mut first).await;
        task_observed.lock().unwrap().push(request);
        let extra_headers: String = first_headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let head = format!(
            "HTTP/1.1 200 OK\r\n{extra_headers}Content-Length: {first_declared_length}\r\nConnection: close\r\n\r\n"
        );
        first.write_all(head.as_bytes()).await.unwrap();
        first.write_all(&first_body).await.unwrap();
        first.shutdown().await.unwrap();

        let (mut second, _) = listener.accept().await.unwrap();
        let request = read_request(&mut second).await;
        task_observed.lock().unwrap().push(request);
        let reason = if second_status == 206 {
            "Partial Content"
        } else {
            "OK"
        };
        let extra_headers: String = second_headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let head = format!(
            "HTTP/1.1 {second_status} {reason}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            second_body.len()
        );
        second.write_all(head.as_bytes()).await.unwrap();
        let _ = second.write_all(&second_body).await;
        let _ = second.shutdown().await;
    });
    (format!("http://{address}/"), task, observed)
}

async fn barrier_server(requests: usize) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let barrier = Arc::new(Barrier::new(requests));
    let task = tokio::spawn(async move {
        let mut handlers = Vec::new();
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().await.unwrap();
            let barrier = barrier.clone();
            handlers.push(tokio::spawn(async move {
                let request = read_request(&mut stream).await;
                let id = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|path| path.trim_end_matches('?').rsplit('/').next())
                    .and_then(|id| id.parse::<u64>().ok())
                    .unwrap();
                barrier.wait().await;
                let body = format!(r#"{{"remote":{id}}}"#);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
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
        let chunked = headers.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("transfer-encoding")
                    && value
                        .split(',')
                        .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
            })
        });
        if chunked {
            if request[header_end + 4..]
                .windows(5)
                .any(|value| value == b"0\r\n\r\n")
            {
                break;
            }
            continue;
        }
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}
use std::sync::{Arc, Mutex as StdMutex};

#[tokio::test]
async fn an_explicit_null_fixed_parameter_is_sent_in_a_json_body() {
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
                    {"name": "cleared", "mode": "fixed", "value": null, "location": "body"},
                    {"name": "kept", "mode": "fixed", "value": 1, "location": "body"}
                ]
            }
        }),
    )
    .await;
    server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    let request = observed.lock().unwrap()[0].clone();
    assert!(request.contains(r#""cleared":null"#), "{request}");
    assert!(request.contains(r#""kept":1"#), "{request}");
}

#[tokio::test]
async fn a_fixed_parameter_without_a_value_is_rejected() {
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": "http://127.0.0.1:9/",
                "method": "POST",
                "parameters": [{"name": "missing", "mode": "fixed", "required": false}]
            }
        }),
    )
    .await;
    assert_eq!(result["status"], "failed");
    assert_eq!(result["errors"][0]["code"], "INVALID_INPUT");
}

#[tokio::test]
async fn null_is_refused_where_a_parameter_is_rendered_as_text() {
    let engine = local_engine();
    let cases = [
        (
            "GET",
            "http://127.0.0.1:9/{id}",
            json!({"name": "id", "mode": "fixed", "value": null}),
        ),
        (
            "GET",
            "http://127.0.0.1:9/",
            json!({"name": "q", "mode": "fixed", "value": null}),
        ),
        (
            "GET",
            "http://127.0.0.1:9/",
            json!({"name": "q", "mode": "fixed", "value": ["a", null], "location": "query"}),
        ),
        (
            "GET",
            "http://127.0.0.1:9/",
            json!({"name": "X-Flag", "mode": "fixed", "value": null, "location": "header"}),
        ),
        (
            "GET",
            "http://127.0.0.1:9/",
            json!({"name": "sid", "mode": "fixed", "value": null, "location": "cookie"}),
        ),
    ];
    for (method, url, parameter) in cases {
        let result = execute(
            &engine,
            json!({
                "schema_version": 1,
                "operation": "test",
                "connection": {"url": url, "method": method, "parameters": [parameter]}
            }),
        )
        .await;
        assert_eq!(result["status"], "failed", "{parameter}");
        assert_eq!(result["errors"][0]["code"], "INVALID_INPUT", "{parameter}");
    }

    for body_type in ["form_urlencoded", "multipart"] {
        let result = execute(
            &engine,
            json!({
                "schema_version": 1,
                "operation": "test",
                "connection": {
                    "url": "http://127.0.0.1:9/",
                    "method": "POST",
                    "request": {"body_type": body_type},
                    "parameters": [{"name": "f", "mode": "fixed", "value": null, "location": "body"}]
                }
            }),
        )
        .await;
        assert_eq!(result["errors"][0]["code"], "INVALID_INPUT", "{body_type}");
    }

    let raw = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": "http://127.0.0.1:9/",
                "method": "POST",
                "request": {"body_type": "raw", "raw_body": "<v>{f}</v>"},
                "parameters": [{"name": "f", "mode": "fixed", "value": null, "location": "body"}]
            }
        }),
    )
    .await;
    assert_eq!(raw["errors"][0]["code"], "INVALID_INPUT");
}

#[tokio::test]
async fn a_mapped_null_from_the_input_is_refused_in_the_query() {
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "enrich",
            "connection": {
                "url": "http://127.0.0.1:9/",
                "method": "GET",
                "parameters": [{"name": "id", "required": true}]
            },
            "input": {"records": [{"id": null}]}
        }),
    )
    .await;
    assert_eq!(result["errors"][0]["code"], "INVALID_INPUT", "{result}");
}

/// Runs a single-record `generate` whose response is `body` and returns the
/// result after applying `transforms` to the columns `a` and `b`.
async fn transform_result(body: &str, transforms: Value) -> Value {
    let (url, server, _) =
        owned_recorded_server(vec![(200, body.as_bytes().to_vec(), vec![])]).await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {
                    "output_mapping": [
                        {"path": "a", "column": "a"},
                        {"path": "b", "column": "b"}
                    ],
                    "transforms": transforms
                }
            }
        }),
    )
    .await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn unrepresentable_numeric_results_fail_instead_of_becoming_null() {
    let cases = [
        // A product beyond u64 has no exact JSON integer.
        (
            format!(r#"{{"a":{}}}"#, u64::MAX),
            json!({"source": "a", "column": "c", "operation": "multiply", "value": u64::MAX}),
        ),
        // Mixed integer/float arithmetic would start from a rounded 2^53 + 1.
        (
            r#"{"a":9007199254740993}"#.to_owned(),
            json!({"source": "a", "column": "c", "operation": "subtract", "value": 9007199254740992.0}),
        ),
        // A fractional quotient of an integer beyond 2^53 would be computed
        // from a rounded dividend.
        (
            r#"{"a":9007199254740995}"#.to_owned(),
            json!({"source": "a", "column": "c", "operation": "divide", "value": 3}),
        ),
        // An integer string wider than i128 must not be parsed as a float.
        (
            r#"{"a":"170141183460469231731687303715884105729"}"#.to_owned(),
            json!({"source": "a", "column": "c", "operation": "subtract", "value": 1}),
        ),
        // A float overflow has no JSON spelling.
        (
            r#"{"a":1e308}"#.to_owned(),
            json!({"source": "a", "column": "c", "operation": "multiply", "value": 10}),
        ),
    ];
    for (body, transform) in cases {
        let result = transform_result(&body, json!([transform])).await;
        assert_eq!(result["status"], "failed", "{transform}: {result}");
        assert_eq!(
            result["errors"][0]["code"], "INVALID_RESPONSE",
            "{transform}"
        );
    }
}

#[tokio::test]
async fn values_a_transform_cannot_handle_fail_the_row() {
    let cases = [
        (
            r#"{"a":"abc"}"#,
            json!({"source": "a", "column": "c", "operation": "add", "value": 1}),
        ),
        (
            r#"{"a":{"x":1}}"#,
            json!({"source": "a", "column": "c", "operation": "round"}),
        ),
        (
            r#"{"a":5}"#,
            json!({"source": "a", "column": "c", "operation": "uppercase"}),
        ),
        (
            r#"{"a":[1]}"#,
            json!({"source": "a", "column": "c", "operation": "prefix", "value": "x"}),
        ),
        (
            r#"{"a":true}"#,
            json!({"source": "a", "column": "c", "operation": "kelvin_to_celsius"}),
        ),
    ];
    for (body, transform) in cases {
        let result = transform_result(body, json!([transform])).await;
        assert_eq!(result["status"], "failed", "{transform}: {result}");
        assert_eq!(
            result["errors"][0]["code"], "INVALID_RESPONSE",
            "{transform}"
        );
    }
}

#[tokio::test]
async fn null_propagates_through_transforms() {
    let transforms = json!([
        {"source": "a", "column": "sum", "operation": "add", "value": 1},
        {"source": "a", "column": "upper", "operation": "uppercase"},
        {"source": "a", "column": "prefixed", "operation": "prefix", "value": "id-"},
        {"source": "a", "column": "replaced", "operation": "replace", "value": {"find": "a", "replace": "b"}},
        {"source": "a", "column": "defaulted", "operation": "default_if_null", "value": 0}
    ]);
    let result = transform_result(r#"{"a":null}"#, transforms).await;
    assert_eq!(result["status"], "success", "{result}");
    let record = &result["output"]["records"][0];
    for column in ["sum", "upper", "prefixed", "replaced"] {
        assert_eq!(record[column], Value::Null, "{column}");
    }
    assert_eq!(record["defaulted"], json!(0));
}

#[tokio::test]
async fn a_condition_on_a_missing_or_null_column_does_not_apply() {
    let transforms = json!([
        {"source": "b", "column": "eq", "operation": "default_if_null", "value": "set", "condition": "a == ''"},
        {"source": "b", "column": "ne", "operation": "default_if_null", "value": "set", "condition": "a != 'x'"},
        {"source": "b", "column": "hit", "operation": "default_if_null", "value": "set", "condition": "b != 'x'"}
    ]);
    let result = transform_result(r#"{"a":null,"b":"y"}"#, transforms).await;
    assert_eq!(result["status"], "success", "{result}");
    let record = &result["output"]["records"][0];
    assert_eq!(record.get("eq"), None, "{record}");
    assert_eq!(record.get("ne"), None, "{record}");
    assert_eq!(record["hit"], json!("y"));
}

#[tokio::test]
async fn invalid_transforms_are_rejected_before_any_request() {
    let invalid = [
        json!({"source": "a", "column": "c", "operation": "explode"}),
        json!({"source": "a", "column": "", "operation": "uppercase"}),
        json!({"source": "", "column": "c", "operation": "uppercase"}),
        json!({"source": "a", "column": "c", "operation": "add"}),
        json!({"source": "a", "column": "c", "operation": "add", "value": "many"}),
        json!({"source": "a", "column": "c", "operation": "divide", "value": 0}),
        json!({"source": "a", "column": "c", "operation": "divide", "value": "0.0"}),
        json!({"source": "a", "column": "c", "operation": "round", "value": 99}),
        json!({"source": "a", "column": "c", "operation": "round", "value": 1.5}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "value": 1}),
        json!({"source": "a", "column": "c", "operation": "prefix"}),
        json!({"source": "a", "column": "c", "operation": "prefix", "value": null}),
        json!({"source": "a", "column": "c", "operation": "replace", "value": {"find": ""}}),
        json!({"source": "a", "column": "c", "operation": "default_if_null"}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "a"}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "status == 'active"}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "status == active'"}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "status == 'a'b'"}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "status == 'a\""}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "status =="}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "== 'x'"}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "a == b == c"}),
        json!({"source": "a", "column": "c", "operation": "uppercase", "condition": "a != b == c"}),
    ];
    for transform in invalid {
        // Port 9 is never contacted: validation fails first.
        let result = execute(
            &local_engine(),
            json!({
                "schema_version": 1,
                "operation": "generate",
                "connection": {
                    "url": "http://127.0.0.1:9/",
                    "method": "GET",
                    "response": {"transforms": [transform]}
                }
            }),
        )
        .await;
        assert_eq!(result["status"], "failed", "{transform}");
        assert_eq!(
            result["errors"][0]["code"], "INVALID_INPUT",
            "{transform}: {result}"
        );
        assert_eq!(result["metrics"]["requests"], 0, "{transform}");
    }
}

#[tokio::test]
async fn a_flat_array_batch_refuses_records_without_exactly_one_value() {
    let (url, server, observed) =
        recorded_server(vec![(200, r#"{"results":[{"ok":1}]}"#, vec![])]).await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "enrich",
            "connection": {
                "url": url,
                "method": "POST",
                "batch": {
                    "enabled": true,
                    "input_key": "ids",
                    "input_format": "flat_array",
                    "output_path": "results"
                }
            },
            "input": {"records": [{"id": 1}, {"id": null}, {"id": 3, "name": "x"}]}
        }),
    )
    .await;
    server.await.unwrap();

    let indexes = result["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| (error["input_index"].clone(), error["code"].clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        indexes,
        [
            (json!(1), json!("INVALID_INPUT")),
            (json!(2), json!("INVALID_INPUT"))
        ],
        "{result}"
    );
    let request = observed.lock().unwrap()[0].clone();
    assert!(request.contains(r#"{"ids":[1]}"#), "{request}");
}

#[tokio::test]
async fn malformed_json_paths_are_rejected_before_any_request() {
    // A malformed path never resolves, so at run time it would read as a
    // response that lacks the field (null, or the mapping default) instead of
    // a configuration mistake. Every path the connection reads is checked.
    let malformed = "data[0";
    let connections = [
        json!({"response": {"records_path": malformed}}),
        json!({"response": {"error_path": malformed}}),
        json!({"response": {"output_mapping": [{"path": malformed, "column": "c"}]}}),
        json!({"response": {"output_mapping": [{"path": "items[]", "column": "c"}]}}),
        json!({"response": {"iterate_on": [{"path": malformed, "as": "item"}]}}),
        json!({"batch": {"output_path": malformed}}),
        json!({"polling": {"id_path": malformed}}),
        json!({"polling": {"status_path": malformed}}),
        json!({"polling": {"url_path": malformed}}),
        json!({"polling": {"result_path": malformed}}),
        json!({"polling": {"result_url_path": malformed}}),
        json!({"pagination": {"type": "cursor", "cursor_path": malformed}}),
        json!({"pagination": {"type": "link", "link_path": malformed}}),
    ];
    for extra in connections {
        let mut connection = json!({"url": "http://127.0.0.1:9/", "method": "GET"});
        for (key, value) in extra.as_object().unwrap() {
            connection[key] = value.clone();
        }
        // Port 9 is never contacted: validation fails first.
        let result = execute(
            &local_engine(),
            json!({
                "schema_version": 1,
                "operation": "generate",
                "connection": connection,
            }),
        )
        .await;
        assert_eq!(result["status"], "failed", "{extra}");
        assert_eq!(
            result["errors"][0]["code"], "INVALID_INPUT",
            "{extra}: {result}"
        );
        assert_eq!(result["metrics"]["requests"], 0, "{extra}");
    }
}

#[tokio::test]
async fn a_null_transform_argument_is_rejected_before_any_request() {
    for transform in [
        json!({"source": "a", "column": "c", "operation": "prefix", "value": null}),
        json!({"source": "a", "column": "c", "operation": "suffix", "value": null}),
        json!({"source": "a", "column": "c", "operation": "add", "value": null}),
    ] {
        let result = execute(
            &local_engine(),
            json!({
                "schema_version": 1,
                "operation": "generate",
                "connection": {
                    "url": "http://127.0.0.1:9/",
                    "method": "GET",
                    "response": {"transforms": [transform]}
                }
            }),
        )
        .await;
        assert_eq!(
            result["errors"][0]["code"], "INVALID_INPUT",
            "{transform}: {result}"
        );
        assert_eq!(result["metrics"]["requests"], 0, "{transform}");
    }
}

#[tokio::test]
async fn null_is_not_read_as_empty_text_in_transforms() {
    let (url, server, _) = recorded_server(vec![(200, r#"{"a":null,"b":"ABC"}"#, vec![])]).await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {
                    "output_mapping": [
                        {"path": "a", "column": "a"},
                        {"path": "b", "column": "b"}
                    ],
                    "transforms": [
                        {"source": "a", "column": "prefixed", "operation": "prefix", "value": "id-"},
                        {"source": "a", "column": "suffixed", "operation": "suffix", "value": "-x"},
                        {"source": "a", "column": "replaced", "operation": "replace",
                         "value": {"find": "x", "replace": "y"}},
                        {"source": "b", "column": "matched", "operation": "uppercase",
                         "condition": "a == ''"}
                    ]
                }
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(result["status"], "success", "{result}");
    let record = &result["output"]["records"][0];
    assert_eq!(record["prefixed"], Value::Null, "{record}");
    assert_eq!(record["suffixed"], Value::Null, "{record}");
    assert_eq!(record["replaced"], Value::Null, "{record}");
    assert_eq!(record.get("matched"), None, "{record}");
}

#[tokio::test]
async fn a_null_poll_status_is_not_read_as_an_empty_status() {
    // An empty pending value is legal on the Rust surface; a null status must
    // not match it as if null were "".
    let (base_url, server, _) = recorded_server(vec![
        (202, r#"{"accepted":true}"#, vec![("Location", "/jobs/1")]),
        (200, r#"{"status":null}"#, vec![]),
    ])
    .await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": format!("{base_url}jobs"),
                "method": "POST",
                "polling": {
                    "pending_values": [""],
                    "interval_ms": 0,
                    "max_attempts": 3
                }
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(result["errors"][0]["code"], "INVALID_RESPONSE", "{result}");
    assert_eq!(result["metrics"]["poll_requests"], 1);
}

#[tokio::test]
async fn a_null_job_id_is_not_rendered_into_the_poll_url() {
    let (base_url, server, observed) =
        recorded_server(vec![(202, r#"{"id":null,"status":"queued"}"#, vec![])]).await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": format!("{base_url}jobs"),
                "method": "POST",
                "polling": {
                    "url_template": "{base}/jobs/{id}",
                    "location_header": null,
                    "interval_ms": 0,
                    "max_attempts": 3
                }
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(result["errors"][0]["code"], "INVALID_RESPONSE", "{result}");
    assert_eq!(
        observed.lock().unwrap().len(),
        1,
        "no poll may reach /jobs/"
    );
}

#[tokio::test]
async fn well_formed_conditions_still_apply() {
    let transforms = json!([
        {"source": "b", "column": "single", "operation": "uppercase", "condition": "a == 'on'"},
        {"source": "b", "column": "double", "operation": "uppercase", "condition": "a == \"on\""},
        {"source": "b", "column": "bare", "operation": "uppercase", "condition": "a==on"},
        {"source": "b", "column": "empty", "operation": "uppercase", "condition": "a != ''"},
        {"source": "b", "column": "skipped", "operation": "uppercase", "condition": "a != 'on'"}
    ]);
    let result = transform_result(r#"{"a":"on","b":"x"}"#, transforms).await;
    assert_eq!(result["status"], "success", "{result}");
    let record = &result["output"]["records"][0];
    for column in ["single", "double", "bare", "empty"] {
        assert_eq!(record[column], json!("X"), "{column}: {record}");
    }
    assert_eq!(record.get("skipped"), None, "{record}");
}

#[tokio::test]
async fn operators_inside_a_quoted_literal_are_part_of_the_literal() {
    let transforms = json!([
        {"source": "b", "column": "eq_single", "operation": "uppercase", "condition": "a == 'x==y!=z'"},
        {"source": "b", "column": "eq_double", "operation": "uppercase", "condition": "a == \"x==y!=z\""},
        {"source": "b", "column": "ne_single", "operation": "uppercase", "condition": "a != 'a==b'"},
        {"source": "b", "column": "ne_double", "operation": "uppercase", "condition": "a != \"a!=b\""},
        {"source": "b", "column": "eq_miss", "operation": "uppercase", "condition": "a == 'x==y'"},
        {"source": "b", "column": "ne_miss", "operation": "uppercase", "condition": "a != \"x==y!=z\""}
    ]);
    let result = transform_result(r#"{"a":"x==y!=z","b":"v"}"#, transforms).await;
    assert_eq!(result["status"], "success", "{result}");
    let record = &result["output"]["records"][0];
    for column in ["eq_single", "eq_double", "ne_single", "ne_double"] {
        assert_eq!(record[column], json!("V"), "{column}: {record}");
    }
    for column in ["eq_miss", "ne_miss"] {
        assert_eq!(record.get(column), None, "{column}: {record}");
    }
}

#[tokio::test]
async fn a_withheld_idempotency_header_also_withdraws_non_idempotent_retries() {
    // `X-Deduplication-ID` does not name an idempotency key, so it is not
    // carried to another origin. The retries it enabled must not be carried
    // either: a POST retried without its key can execute twice.
    let (second_url, second_server, second_observed) = recorded_server(vec![
        (503, r#"{"error":"busy"}"#, vec![]),
        (200, r#"{"items":[{"id":2}]}"#, vec![]),
    ])
    .await;
    let first_body = format!(r#"{{"items":[{{"id":1}}],"next":"{second_url}"}}"#);
    let (first_url, first_server, _) = owned_recorded_server(vec![(
        200,
        first_body.into_bytes(),
        vec![("Content-Type", "application/json")],
    )])
    .await;

    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": first_url,
                "method": "POST",
                "response": {"records_path": "items"},
                "retry": {"max_attempts": 3, "backoff_base_ms": 1, "max_backoff_ms": 1},
                "idempotency": {"name": "X-Deduplication-ID", "location": "header"},
                "pagination": {
                    "type": "link",
                    "link_path": "next",
                    "max_pages": 2,
                    "allow_cross_origin": true
                }
            },
            "options": {"idempotency_key": "page-key-1"}
        }),
    )
    .await;
    first_server.await.unwrap();
    second_server.abort();

    assert_eq!(result["status"], "failed", "{result}");
    let second = second_observed.lock().unwrap().clone();
    assert_eq!(second.len(), 1, "the unprotected POST must not be retried");
    assert!(
        !second[0]
            .to_ascii_lowercase()
            .contains("x-deduplication-id")
    );
}

fn cookie_engine(max_cookie_sessions: usize) -> Engine {
    Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        max_cookie_sessions,
        ..EngineConfig::default()
    })
}

fn with_session(url: &str, session: &str) -> Value {
    json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {"url": url, "method": "GET", "cookies": {"session": session}}
    })
}

/// Asserts that a request on `session` is refused before any network activity.
/// Port 9 is never contacted: a refusal that reached the network would show up
/// as a transport error instead of a policy violation.
async fn assert_refused_before_network(engine: &Engine, session: &str) {
    let result = execute(engine, with_session("http://127.0.0.1:9/", session)).await;
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["errors"][0]["code"], "POLICY_VIOLATION", "{result}");
    assert_eq!(result["metrics"]["requests"], 0, "{result}");
}

#[tokio::test]
async fn an_open_cookie_session_keeps_its_cookies() {
    let (url, server, observed) = recorded_server(vec![
        (
            200,
            r#"{"ok":true}"#,
            vec![("Set-Cookie", "sid=login; Path=/")],
        ),
        (200, r#"{"ok":true}"#, vec![]),
    ])
    .await;
    let engine = cookie_engine(4);
    let session = engine.open_cookie_session().await.unwrap().to_token();
    assert_eq!(
        execute(&engine, with_session(&url, &session)).await["status"],
        "success"
    );
    assert_eq!(
        execute(&engine, with_session(&url, &session)).await["status"],
        "success"
    );
    server.await.unwrap();
    assert!(
        observed.lock().unwrap()[1]
            .to_ascii_lowercase()
            .contains("cookie: sid=login")
    );
}

#[tokio::test]
async fn a_closed_cookie_session_is_refused_and_not_recreated() {
    let engine = cookie_engine(4);
    let session = engine.open_cookie_session().await.unwrap();
    engine.close_cookie_session(&session).await.unwrap();
    assert_refused_before_network(&engine, &session.to_token()).await;
    // Closing twice is an error too, so a caller that lost track finds out.
    assert!(engine.close_cookie_session(&session).await.is_err());
    // The slot is reused, but under a new generation: the old handle stays
    // refused while the new one works.
    let reopened = engine.open_cookie_session().await.unwrap();
    assert_ne!(reopened, session);
    assert_refused_before_network(&engine, &session.to_token()).await;
}

#[tokio::test]
async fn an_evicted_cookie_session_is_refused_and_not_recreated() {
    let (url, server, _) = recorded_server(vec![(
        200,
        r#"{"ok":true}"#,
        vec![("Set-Cookie", "sid=login; Path=/")],
    )])
    .await;
    let engine = cookie_engine(2);
    let oldest = engine.open_cookie_session().await.unwrap().to_token();
    assert_eq!(
        execute(&engine, with_session(&url, &oldest)).await["status"],
        "success"
    );
    server.await.unwrap();
    let _second = engine.open_cookie_session().await.unwrap();
    // Both slots are taken, so this evicts the least recently used session.
    let _third = engine.open_cookie_session().await.unwrap();
    assert_refused_before_network(&engine, &oldest).await;
}

#[tokio::test]
async fn more_than_ten_thousand_sequential_sessions_never_exhaust_the_engine() {
    // Memory is bounded by the slots, not by how many sessions ever existed:
    // sessions that are closed, and sessions that are simply abandoned and
    // evicted, both leave nothing behind.
    let engine = cookie_engine(8);
    let mut abandoned = Vec::new();
    for index in 0..10_500 {
        let session = engine.open_cookie_session().await.unwrap();
        if index % 2 == 0 {
            engine.close_cookie_session(&session).await.unwrap();
        } else if abandoned.len() < 4 {
            abandoned.push(session);
        }
    }
    for session in &abandoned {
        assert_refused_before_network(&engine, &session.to_token()).await;
    }
    let (url, server, observed) = recorded_server(vec![
        (
            200,
            r#"{"ok":true}"#,
            vec![("Set-Cookie", "sid=last; Path=/")],
        ),
        (200, r#"{"ok":true}"#, vec![]),
    ])
    .await;
    let last = engine.open_cookie_session().await.unwrap().to_token();
    assert_eq!(
        execute(&engine, with_session(&url, &last)).await["status"],
        "success"
    );
    assert_eq!(
        execute(&engine, with_session(&url, &last)).await["status"],
        "success"
    );
    server.await.unwrap();
    assert!(
        observed.lock().unwrap()[1]
            .to_ascii_lowercase()
            .contains("cookie: sid=last")
    );
}

#[tokio::test]
async fn a_cookie_session_from_another_engine_or_forged_is_refused() {
    let issuer = cookie_engine(4);
    let engine = cookie_engine(4);
    let foreign = issuer.open_cookie_session().await.unwrap();
    assert_refused_before_network(&engine, &foreign.to_token()).await;
    assert!(engine.close_cookie_session(&foreign).await.is_err());

    // Same engine, slot and generation, but not the random part the engine
    // issued: a handle cannot be assembled from guessable numbers.
    let genuine = engine.open_cookie_session().await.unwrap().to_token();
    let (prefix, nonce) = genuine.rsplit_once('.').unwrap();
    let flipped = if nonce.starts_with('0') { "1" } else { "0" };
    let forged = format!("{prefix}.{flipped}{}", &nonce[1..]);
    assert_refused_before_network(&engine, &forged).await;
}

#[tokio::test]
async fn a_malformed_cookie_session_handle_is_invalid_input() {
    // A handle that does not parse fails the request contract itself, before
    // anything is executed.
    let engine = cookie_engine(4);
    let malformed = [
        with_session("http://127.0.0.1:9/", ""),
        with_session("http://127.0.0.1:9/", "default"),
        with_session("http://127.0.0.1:9/", "rcs1.zz.0.0.0"),
        with_session(
            "http://127.0.0.1:9/",
            "rcs1.0000000000000000.01.0.0000000000000000",
        ),
        // The former shape of the cookie policy is refused, not ignored.
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": "http://127.0.0.1:9/",
                "method": "GET",
                "cookies": {"enabled": true, "jar_id": "tenant"}
            }
        }),
    ];
    for request in malformed {
        let error = engine
            .execute_json(&request.to_string())
            .await
            .expect_err("a malformed cookie policy must be refused");
        assert!(matches!(error, EngineError::InvalidInput(_)), "{request}");
    }
}

#[tokio::test]
async fn closing_a_session_during_oauth_keeps_the_admitted_request_and_refuses_new_ones() {
    // The resource server sets a cookie on the first request, then expects the
    // in-flight request to arrive with it.
    let (resource_url, resource_server, resource_observed) = recorded_server(vec![
        (
            200,
            r#"{"step":1}"#,
            vec![("Set-Cookie", "sid=held; Path=/")],
        ),
        (200, r#"{"step":2}"#, vec![]),
    ])
    .await;

    // The token endpoint accepts, announces the arrival, and answers only when
    // the test says so: the request is held inside OAuth, after admission and
    // after network activity has started.
    let token_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/token", token_listener.local_addr().unwrap());
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let token_server = tokio::spawn(async move {
        let (mut stream, _) = token_listener.accept().await.unwrap();
        let _ = read_request(&mut stream).await;
        let _ = arrived.send(());
        released.await.unwrap();
        let body = r#"{"access_token":"t","token_type":"Bearer","expires_in":3600}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    let engine = Arc::new(cookie_engine(1));
    let session = engine.open_cookie_session().await.unwrap();
    let token = session.to_token();
    let first = execute(&engine, with_session(&resource_url, &token)).await;
    assert_eq!(first["status"], "success", "{first}");

    let mut with_oauth = with_session(&resource_url, &token);
    with_oauth["connection"]["auth"] = json!({
        "type": "oauth2_client_credentials",
        "token_url": token_url,
        "client_id": "client",
        "client_secret": "secret"
    });
    let in_flight_engine = Arc::clone(&engine);
    let in_flight = tokio::spawn(async move { execute(&in_flight_engine, with_oauth).await });
    timeout(Duration::from_secs(5), arrival)
        .await
        .expect("the request never reached the token endpoint")
        .unwrap();

    engine.close_cookie_session(&session).await.unwrap();
    // New requests are refused at once, before any network activity.
    assert_refused_before_network(&engine, &token).await;
    // The only slot is still held by the in-flight request: it is not reused.
    assert!(engine.open_cookie_session().await.is_err());

    release.send(()).unwrap();
    let completed = in_flight.await.unwrap();
    token_server.await.unwrap();
    resource_server.await.unwrap();
    assert_eq!(completed["status"], "success", "{completed}");
    assert!(
        resource_observed.lock().unwrap()[1]
            .to_ascii_lowercase()
            .contains("cookie: sid=held"),
        "the in-flight request must keep the session it was admitted with"
    );

    // Released: the slot is reused, under a new generation.
    let reopened = engine.open_cookie_session().await.unwrap();
    assert_ne!(reopened, session);
    assert_refused_before_network(&engine, &token).await;
}

async fn write_json_response(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

/// A listener that records whether anything connected to it within `wait`.
async fn watched_listener(wait: Duration) -> (String, JoinHandle<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let watcher = tokio::spawn(async move { timeout(wait, listener.accept()).await.is_ok() });
    (url, watcher)
}

#[tokio::test]
async fn a_resumed_cross_origin_poll_with_a_stale_session_never_reaches_the_network() {
    // The poll and the cancellation target other origins, so the credential
    // scope strips the session from them before they reach the transport. The
    // stale handle must still refuse the operation up front.
    let (poll_base, poll_watcher) = watched_listener(Duration::from_secs(1)).await;
    let (cancel_base, cancel_watcher) = watched_listener(Duration::from_secs(1)).await;
    let engine = cookie_engine(4);
    let session = engine.open_cookie_session().await.unwrap();
    engine.close_cookie_session(&session).await.unwrap();

    let result = execute(
        &engine,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": "http://127.0.0.1:9/submit",
                "method": "POST",
                "cookies": {"session": session},
                "polling": {
                    "url_template": format!("{poll_base}/jobs/{{job_id}}"),
                    "allow_cross_origin": true,
                    "interval_ms": 0,
                    "max_attempts": 1,
                    "resume": {"job_id": "existing-1"},
                    "cancel": {
                        "url_template": format!("{cancel_base}/jobs/{{job_id}}"),
                        "on_poll_timeout": true
                    }
                }
            }
        }),
    )
    .await;

    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["errors"][0]["code"], "POLICY_VIOLATION", "{result}");
    assert_eq!(result["metrics"]["requests"], 0, "{result}");
    assert!(!poll_watcher.await.unwrap(), "the poll must not be sent");
    assert!(
        !cancel_watcher.await.unwrap(),
        "the cancellation must not be sent"
    );
}

#[tokio::test]
async fn a_remote_cancellation_after_the_session_closed_is_not_sent() {
    // The session is valid when the operation starts and is closed while the
    // poll is in flight. The poll times out and triggers a remote cancellation
    // on another origin, where the scope strips the session: that request
    // belongs to an ended session and must be refused before the network.
    let owner = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner_url = format!("http://{}", owner.local_addr().unwrap());
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let owner_server = tokio::spawn(async move {
        let (mut submit, _) = owner.accept().await.unwrap();
        let _ = read_request(&mut submit).await;
        write_json_response(
            &mut submit,
            "202 Accepted",
            r#"{"id":"job-1","status":"queued"}"#,
        )
        .await;
        let (mut poll, _) = owner.accept().await.unwrap();
        let _ = read_request(&mut poll).await;
        let _ = arrived.send(());
        released.await.unwrap();
        write_json_response(&mut poll, "200 OK", r#"{"status":"running"}"#).await;
    });
    let (cancel_base, cancel_watcher) = watched_listener(Duration::from_secs(3)).await;

    let engine = Arc::new(cookie_engine(4));
    let session = engine.open_cookie_session().await.unwrap();
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{owner_url}/submit"),
            "method": "POST",
            "cookies": {"session": session},
            "polling": {
                "url_template": format!("{owner_url}/jobs/{{job_id}}"),
                "location_header": null,
                "allow_cross_origin": true,
                "interval_ms": 0,
                "max_attempts": 1,
                "cancel": {
                    "url_template": format!("{cancel_base}/jobs/{{job_id}}"),
                    "on_poll_timeout": true
                }
            }
        }
    });
    let running_engine = Arc::clone(&engine);
    let running = tokio::spawn(async move { execute(&running_engine, request).await });
    timeout(Duration::from_secs(5), arrival)
        .await
        .expect("the poll never arrived")
        .unwrap();
    engine.close_cookie_session(&session).await.unwrap();
    release.send(()).unwrap();

    let result = running.await.unwrap();
    owner_server.await.unwrap();
    assert_eq!(result["errors"][0]["code"], "POLLING_TIMEOUT", "{result}");
    assert_eq!(result["metrics"]["requests"], 2, "{result}");
    assert_eq!(
        result["recoveries"][0]["cancel_requested"], true,
        "{result}"
    );
    assert_eq!(
        result["recoveries"][0]["cancel_accepted"], false,
        "{result}"
    );
    assert!(
        !cancel_watcher.await.unwrap(),
        "a cancellation for an ended session must not be sent"
    );
}

#[tokio::test]
async fn a_stale_session_is_refused_before_the_idempotency_key_is_recorded() {
    // The operation-level check runs before idempotency admission: a refused
    // attempt must not record the key, or retrying the same key with a fresh
    // session would be reported as a conflicting reuse.
    let (url, server, _) = recorded_server(vec![(200, r#"{"ok":true}"#, vec![])]).await;
    let engine = cookie_engine(4);
    let stale = engine.open_cookie_session().await.unwrap();
    engine.close_cookie_session(&stale).await.unwrap();
    let attempt = |session: &CookieSession| {
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {"url": url, "method": "POST", "cookies": {"session": session}},
            "options": {"idempotency_key": "retry-with-new-session"}
        })
    };

    let refused = execute(&engine, attempt(&stale)).await;
    assert_eq!(
        refused["errors"][0]["code"], "POLICY_VIOLATION",
        "{refused}"
    );
    assert_eq!(refused["metrics"]["requests"], 0);

    let fresh = engine.open_cookie_session().await.unwrap();
    let retried = execute(&engine, attempt(&fresh)).await;
    server.await.unwrap();
    assert_eq!(retried["status"], "success", "{retried}");
}

#[tokio::test]
async fn numeric_settings_without_a_meaning_are_refused_not_replaced() {
    // Each of these used to be replaced silently (a rate falling back to the
    // engine rate, zero attempts becoming one, zero pages paginating nothing
    // with a success). Port 9 is never contacted: validation fails first.
    let connections = [
        json!({"requests_per_second": 0.0}),
        json!({"requests_per_second": -5.0}),
        json!({"request": {"timeout_ms": 0}}),
        json!({"retry": {"max_attempts": 0}}),
        json!({"retry": {"backoff_factor": 0.5}}),
        json!({"pagination": {"type": "page", "max_rows": 0}}),
        json!({"pagination": {"type": "cursor", "max_pages": 0}}),
        json!({"pagination": {"type": "link", "max_rows": 0}}),
    ];
    for extra in connections {
        let mut connection = json!({"url": "http://127.0.0.1:9/", "method": "GET"});
        for (key, value) in extra.as_object().unwrap() {
            connection[key] = value.clone();
        }
        let result = execute(
            &local_engine(),
            json!({"schema_version": 1, "operation": "generate", "connection": connection}),
        )
        .await;
        assert_eq!(
            result["errors"][0]["code"], "INVALID_INPUT",
            "{extra}: {result}"
        );
        assert_eq!(result["metrics"]["requests"], 0, "{extra}");
    }

    // A non-finite rate cannot be written in JSON; through the Rust API it
    // is refused the same way.
    let mut request: ExecutionRequest = serde_json::from_value(json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {"url": "http://127.0.0.1:9/", "method": "GET"}
    }))
    .unwrap();
    request.connection.requests_per_second = Some(f64::NAN);
    let result = serde_json::to_value(local_engine().execute(request.clone()).await).unwrap();
    assert_eq!(result["errors"][0]["code"], "INVALID_INPUT", "{result}");
    request.connection.retry.backoff_factor = f64::INFINITY;
    request.connection.requests_per_second = None;
    let result = serde_json::to_value(local_engine().execute(request.clone()).await).unwrap();
    assert_eq!(result["errors"][0]["code"], "INVALID_INPUT", "{result}");

    // Engine settings: refused by every execution instead of being replaced.
    request.connection.retry.backoff_factor = 2.0;
    for config in [
        EngineConfig {
            max_concurrent_requests: 0,
            ..EngineConfig::default()
        },
        EngineConfig {
            requests_per_second: Some(0),
            ..EngineConfig::default()
        },
        EngineConfig {
            connect_timeout_ms: 0,
            ..EngineConfig::default()
        },
        EngineConfig {
            request_timeout_ms: 0,
            ..EngineConfig::default()
        },
    ] {
        let engine = Engine::new(EngineConfig {
            allow_private_networks: true,
            ..config
        });
        let result = serde_json::to_value(engine.execute(request.clone()).await).unwrap();
        assert_eq!(result["errors"][0]["code"], "INVALID_INPUT", "{result}");
        assert_eq!(result["metrics"]["requests"], 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_handles_beyond_the_contract_bound_are_counted_not_dropped() {
    // 130 enrichment records each start a remote job that never finishes; the
    // operation is then cancelled. The result may carry at most 128 recovery
    // handles: the two that do not fit must be reported, since the caller can
    // no longer resume those jobs.
    const JOBS: usize = 130;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let submitted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let polled = std::sync::Arc::new(StdMutex::new(std::collections::BTreeSet::new()));
    let server = {
        let cancellation = cancellation.clone();
        let submitted = submitted.clone();
        let polled = polled.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let cancellation = cancellation.clone();
                let submitted = submitted.clone();
                let polled = polled.clone();
                tokio::spawn(async move {
                    let request = read_request(&mut stream).await;
                    let (status, body, location) = if request.starts_with("POST /submit") {
                        let job = submitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (
                            "202 Accepted",
                            format!(r#"{{"id":"job-{job:03}","status":"queued"}}"#),
                            format!("Location: /jobs/job-{job:03}\r\n"),
                        )
                    } else {
                        let path = request.split_whitespace().nth(1).unwrap_or_default();
                        polled.lock().unwrap().insert(path.to_owned());
                        (
                            "200 OK",
                            r#"{"status":"running"}"#.to_owned(),
                            String::new(),
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    // A job is registered for recovery before its first poll,
                    // so once every job has been polled all of them are known.
                    if polled.lock().unwrap().len() == JOBS {
                        cancellation.cancel();
                    }
                });
            }
        })
    };
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        max_concurrent_requests: 256,
        ..EngineConfig::default()
    });
    let records = (0..JOBS)
        .map(|index| json!({"index": index}))
        .collect::<Vec<_>>();
    let request: ExecutionRequest = serde_json::from_value(json!({
        "schema_version": 1,
        "operation": "enrich",
        "connection": {
            "url": format!("http://{address}/submit"),
            "method": "POST",
            "polling": {"status_path": "status", "interval_ms": 50, "max_attempts": 100_000}
        },
        "input": {"records": records},
        "options": {"enrichment_concurrency": JOBS}
    }))
    .unwrap();

    let result = timeout(
        Duration::from_secs(60),
        engine.execute_with_control(request, ExecutionControl::new(cancellation)),
    )
    .await
    .expect("the cancelled operation finishes");
    server.abort();
    let result = serde_json::to_value(result).unwrap();

    assert_eq!(submitted.load(std::sync::atomic::Ordering::SeqCst), JOBS);
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["errors"][0]["code"], "CANCELLED");
    assert_eq!(result["recoveries"].as_array().unwrap().len(), 128);
    assert_eq!(
        result["errors"][0]["details"]["recoveries_omitted"], 2,
        "{}",
        result["errors"]
    );
}

async fn paginate(pagination: Value, pages: Vec<TestResponse>) -> Value {
    let count = pages.len();
    let (url, server, observed) = recorded_server(pages).await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {"records_path": "items"},
                "pagination": pagination
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(observed.lock().unwrap().len(), count, "{result}");
    result
}

fn assert_limit(result: &Value, rows: Value, details: Value) {
    assert_eq!(result["status"], "partial", "{result}");
    assert_eq!(result["output"]["records"], rows, "{result}");
    let error = &result["errors"][0];
    assert_eq!(error["code"], "PAGINATION_LIMIT_REACHED", "{result}");
    assert_eq!(error["category"], "resource_limit");
    // ERR-014: the pages were requested, so the limit cannot claim that
    // nothing happened remotely.
    assert_eq!(error["phase"], "read");
    assert_eq!(error["remote_effect"], "unknown");
    assert_eq!(error["retry"]["kind"], "requires_recovery");
    assert_eq!(error["details"], details);
    assert_eq!(result["errors"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn rows_cut_by_max_rows_are_a_partial_result_not_a_success() {
    // The second page has two rows but only one fits: the source had more.
    let result = paginate(
        json!({"type": "page", "page_size": 2, "max_rows": 3}),
        vec![
            (200, r#"{"items":[{"v":1},{"v":2}]}"#, vec![]),
            (200, r#"{"items":[{"v":3},{"v":4}]}"#, vec![]),
        ],
    )
    .await;
    assert_limit(
        &result,
        json!([{"v": 1}, {"v": 2}, {"v": 3}]),
        json!({"max_rows": 3}),
    );

    // Offset mode asks for exactly the rows that still fit; a page that fills
    // the request at the limit may hide more rows, and the engine does not
    // send a probe request to find out, so it says the limit was reached.
    let result = paginate(
        json!({"type": "offset", "page_size": 2, "max_rows": 4}),
        vec![
            (200, r#"{"items":[{"v":1},{"v":2}]}"#, vec![]),
            (200, r#"{"items":[{"v":3},{"v":4}]}"#, vec![]),
        ],
    )
    .await;
    assert_limit(
        &result,
        json!([{"v": 1}, {"v": 2}, {"v": 3}, {"v": 4}]),
        json!({"max_rows": 4}),
    );

    // A short page is the end of the data: a complete result.
    let result = paginate(
        json!({"type": "offset", "page_size": 2, "max_rows": 4}),
        vec![
            (200, r#"{"items":[{"v":1},{"v":2}]}"#, vec![]),
            (200, r#"{"items":[{"v":3}]}"#, vec![]),
        ],
    )
    .await;
    assert_eq!(result["status"], "success", "{result}");
    assert_eq!(result["errors"], json!([]));
}

#[tokio::test]
async fn a_next_page_left_by_max_pages_is_a_partial_result() {
    let result = paginate(
        json!({"type": "cursor", "max_pages": 2}),
        vec![
            (200, r#"{"items":[{"v":1}],"next_cursor":"b"}"#, vec![]),
            (200, r#"{"items":[{"v":2}],"next_cursor":"c"}"#, vec![]),
        ],
    )
    .await;
    assert_limit(
        &result,
        json!([{"v": 1}, {"v": 2}]),
        json!({"max_rows": 10_000, "max_pages": 2}),
    );

    let result = paginate(
        json!({"type": "link", "max_pages": 2}),
        vec![
            (200, r#"{"items":[{"v":1}],"next":"/p2"}"#, vec![]),
            (200, r#"{"items":[{"v":2}],"next":"/p3"}"#, vec![]),
        ],
    )
    .await;
    assert_limit(
        &result,
        json!([{"v": 1}, {"v": 2}]),
        json!({"max_rows": 10_000, "max_pages": 2}),
    );

    // max_rows reached with a next Link header still announced.
    let result = paginate(
        json!({"type": "header_link", "max_rows": 1}),
        vec![(
            200,
            r#"{"items":[{"v":1},{"v":2}]}"#,
            vec![("Link", "</p2>; rel=\"next\"")],
        )],
    )
    .await;
    assert_limit(
        &result,
        json!([{"v": 1}]),
        json!({"max_rows": 1, "max_pages": 100}),
    );

    // No next cursor: the data is complete within the limits.
    let result = paginate(
        json!({"type": "cursor", "max_pages": 2}),
        vec![
            (200, r#"{"items":[{"v":1}],"next_cursor":"b"}"#, vec![]),
            (200, r#"{"items":[{"v":2}]}"#, vec![]),
        ],
    )
    .await;
    assert_eq!(result["status"], "success", "{result}");
    assert_eq!(result["output"]["records"], json!([{"v": 1}, {"v": 2}]));
}

#[tokio::test]
async fn a_retry_after_beyond_the_cap_is_not_shortened_into_an_early_retry() {
    // The server asks for 600 s; the policy accepts at most 1 s. Retrying after
    // 1 s would ignore the server, so the 429 is the result.
    let (url, server, observed) = recorded_server(vec![(
        429,
        r#"{"error":"slow down"}"#,
        vec![("Retry-After", "600")],
    )])
    .await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 3, "max_retry_after_ms": 1000}
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(observed.lock().unwrap().len(), 1, "{result}");
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["errors"][0]["code"], "HTTP_STATUS");
    assert_eq!(result["errors"][0]["details"]["http_status"], 429);

    // Within the cap the server's delay is honoured and the retry happens.
    let (url, server, observed) = recorded_server(vec![
        (429, r#"{"error":"slow down"}"#, vec![("Retry-After", "0")]),
        (200, r#"{"ok":true}"#, vec![]),
    ])
    .await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 3, "max_retry_after_ms": 1000}
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(observed.lock().unwrap().len(), 2, "{result}");
    assert_eq!(result["status"], "success", "{result}");
}

#[tokio::test]
async fn page_pagination_keeps_its_page_size_up_to_max_rows() {
    // An ordinary API: page N of size S holds rows (N-1)*S+1 ..= N*S of ten.
    // With page_size 2 and max_rows 3 the second request must still ask for
    // pages of 2, or page 2 of size 1 would return row 2 again.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rows", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        while let Ok(Ok((mut stream, _))) = timeout(Duration::from_secs(2), listener.accept()).await
        {
            let request = read_request(&mut stream).await;
            let line = request.lines().next().unwrap_or_default().to_owned();
            let query = |name: &str| {
                line.split(['?', '&', ' '])
                    .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap()
            };
            let (page, size) = (query("page"), query("page_size"));
            let rows = ((page - 1) * size + 1..=(page * size).min(10))
                .map(|value| json!({"v": value}))
                .collect::<Vec<_>>();
            let body = json!({"items": rows}).to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            requests.push(line);
        }
        requests
    });
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": url,
                "method": "GET",
                "response": {"records_path": "items"},
                "pagination": {"type": "page", "page_size": 2, "max_rows": 3}
            }
        }),
    )
    .await;
    let requests = server.await.unwrap();
    assert_eq!(
        result["output"]["records"],
        json!([{"v": 1}, {"v": 2}, {"v": 3}]),
        "{result} {requests:?}"
    );
    assert!(
        requests.iter().all(|line| line.contains("page_size=2")),
        "{requests:?}"
    );
    assert_eq!(result["status"], "partial");
}

#[test]
fn a_retry_after_too_large_to_represent_is_the_longest_wait() {
    // 18446744073709552 seconds overflow u64 milliseconds. Such a delay is
    // the longest there is: it exceeds any cap, so the 429 is not retried,
    // instead of reading as an absent header and retrying on the backoff.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (url, server, observed) = recorded_server(vec![(
            429,
            r#"{"error":"slow down"}"#,
            vec![("Retry-After", "18446744073709552")],
        )])
        .await;
        let result = execute(
            &local_engine(),
            json!({
                "schema_version": 1,
                "operation": "test",
                "connection": {
                    "url": url,
                    "method": "GET",
                    "retry": {"max_attempts": 2, "backoff_base_ms": 1, "max_backoff_ms": 1}
                }
            }),
        )
        .await;
        // Bounded wait for a second request that must never come.
        let _ = timeout(Duration::from_secs(1), server).await;
        assert_eq!(observed.lock().unwrap().len(), 1, "{result}");
        assert_eq!(result["errors"][0]["code"], "HTTP_STATUS", "{result}");
    });
}

#[tokio::test]
async fn a_link_header_with_a_non_ascii_parameter_still_paginates() {
    // A Link title may carry UTF-8. The header used to be dropped as "not
    // text", which ended pagination after the first page without a word.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for (index, body) in [r#"{"items":[{"v":1}]}"#, r#"{"items":[{"v":2}]}"#]
            .into_iter()
            .enumerate()
        {
            // Bounded: without the fix the second page is never requested.
            let Ok(Ok((mut stream, _))) = timeout(Duration::from_secs(5), listener.accept()).await
            else {
                return;
            };
            let _ = read_request(&mut stream).await;
            let link = if index == 0 {
                "Link: </p2>; rel=\"next\"; title=\"caf\u{e9}\"\r\n"
            } else {
                ""
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{link}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "generate",
            "connection": {
                "url": format!("{base}/p1"),
                "method": "GET",
                "response": {"records_path": "items"},
                "pagination": {"type": "header_link"}
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(
        result["output"]["records"],
        json!([{"v": 1}, {"v": 2}]),
        "{result}"
    );
}

#[tokio::test]
async fn the_request_deadline_binds_every_entry_point() {
    // A server that accepts and never answers: only the deadline ends the call.
    async fn silent() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let mut sink = [0_u8; 64];
            while stream.read(&mut sink).await.unwrap_or(0) > 0 {}
        });
        (url, task)
    }
    let deadline = |millis: i64| {
        let at = time::OffsetDateTime::now_utc() + time::Duration::milliseconds(millis);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            at.year(),
            u8::from(at.month()),
            at.day(),
            at.hour(),
            at.minute(),
            at.second(),
            at.millisecond()
        )
    };
    let (url, server) = silent().await;
    let request: ExecutionRequest = serde_json::from_value(json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {"url": url, "method": "GET"},
        "options": {"deadline": deadline(300)}
    }))
    .unwrap();
    let started = std::time::Instant::now();
    let result = timeout(
        Duration::from_secs(10),
        local_engine().execute_with_control(request, ExecutionControl::default()),
    )
    .await
    .expect("the request deadline ends the call");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(result.errors[0].code, "TIMEOUT");
    server.abort();
}

#[tokio::test]
async fn a_failed_result_counts_the_attempts_that_reached_the_server() {
    // Every attempt reaches the server, which closes the connection without
    // answering. The result fails, and its metrics must still say three
    // requests and two retries: a monitor of amplification reads them.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut seen = 0_usize;
        while seen < 3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            drop(stream);
            seen += 1;
        }
        seen
    });
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "retry": {"max_attempts": 3, "backoff_base_ms": 1, "max_backoff_ms": 1}
            }
        }),
    )
    .await;
    assert_eq!(server.await.unwrap(), 3);
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["metrics"]["requests"], 3, "{result}");
    assert_eq!(result["metrics"]["retries"], 2, "{result}");
}

#[tokio::test]
async fn a_retry_cut_short_by_the_deadline_is_not_counted() {
    // The server asks to retry after 60 s; the deadline ends the execution
    // during that wait. One request went out and no retry did.
    let (url, server, observed) = recorded_server(vec![(
        429,
        r#"{"error":"slow down"}"#,
        vec![("Retry-After", "60")],
    )])
    .await;
    let deadline = {
        let at = time::OffsetDateTime::now_utc() + time::Duration::seconds(1);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            at.year(),
            u8::from(at.month()),
            at.day(),
            at.hour(),
            at.minute(),
            at.second(),
            at.millisecond()
        )
    };
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {"url": url, "method": "GET", "retry": {"max_attempts": 2}},
            "options": {"deadline": deadline}
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(observed.lock().unwrap().len(), 1);
    assert_eq!(result["errors"][0]["code"], "TIMEOUT", "{result}");
    assert_eq!(result["metrics"]["requests"], 1, "{result}");
    assert_eq!(result["metrics"]["retries"], 0, "{result}");
}

#[tokio::test]
async fn a_download_that_cannot_be_written_after_a_post_reports_an_unknown_remote_effect() {
    // The POST reaches the server, which also creates the destination before
    // answering: publishing the download then fails locally. The POST may
    // have changed something remotely, so the failure cannot say `none`.
    let directory = transfer_directory("post-download-write");
    let target = directory.join("export.bin");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/export", listener.local_addr().unwrap());
    let server = {
        let target = target.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            fs::write(&target, b"someone else").await.unwrap();
            let body = b"payload";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            stream.shutdown().await.unwrap();
            request
        })
    };
    let result = execute(
        &transfer_engine(&directory, 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {"url": url, "method": "POST"},
            "input": {"file": {"path": "export.bin"}}
        }),
    )
    .await;
    assert!(server.await.unwrap().starts_with("POST /export "));
    let error = &result["errors"][0];
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(error["code"], "DOWNLOAD_WRITE_FAILED", "{result}");
    assert_eq!(error["category"], "io");
    assert_eq!(error["phase"], "write");
    assert_eq!(error["remote_effect"], "unknown");
    assert_eq!(error["retry"]["kind"], "requires_recovery");
    // The other writer's file is left alone and no staging file remains.
    assert_eq!(fs::read(&target).await.unwrap(), b"someone else");
    assert_no_partial_files(&directory).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn an_expired_deadline_is_refused_before_anything_runs() {
    let request: ExecutionRequest = serde_json::from_value(json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {"url": "http://127.0.0.1:9/", "method": "GET"},
        "options": {"deadline": "2000-01-01T00:00:00Z"}
    }))
    .unwrap();
    let result = serde_json::to_value(local_engine().execute(request).await).unwrap();
    let error = &result["errors"][0];
    assert_eq!(error["code"], "DEADLINE_EXPIRED", "{result}");
    assert_eq!(error["category"], "timeout");
    assert_eq!(error["phase"], "validate");
    assert_eq!(error["remote_effect"], "none");
    assert_eq!(error["retry"]["kind"], "never");
    assert_eq!(result["metrics"]["requests"], 0);
}

#[test]
fn a_deadline_is_any_rfc_3339_spelling_of_utc() {
    // Runtime Binding 1.0 RT-021 (plenora-contracts v1.1.0): every RFC 3339
    // spelling of UTC is accepted, as decision 0010 keeps it in 1.0; a
    // non-zero offset (local time) and `-00:00` (offset unknown) are not UTC.
    for accepted in [
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:00:00z",
        "2099-01-01t00:00:00Z",
        "2099-01-01T00:00:00+00:00",
        "2099-01-01T00:00:00.5Z",
        "2099-01-01T00:00:00.123456789Z",
    ] {
        assert!(
            ExecutionControl::default().with_deadline(accepted).is_ok(),
            "{accepted}"
        );
    }
    for refused in [
        "2099-01-01T02:00:00+02:00",
        "2099-01-01T00:00:00-00:00",
        "2099-01-01T00:00:00",
        "2099-13-01T00:00:00Z",
        "2099-01-01",
        "tomorrow",
    ] {
        let error = ExecutionControl::default()
            .with_deadline(refused)
            .unwrap_err();
        assert_eq!(error.payload().code, "INVALID_INPUT", "{refused}");
    }
}

#[tokio::test]
async fn a_retried_download_whose_staging_cannot_be_reopened_keeps_the_remote_effect_unknown() {
    // A repeatable POST download is cut after a few bytes; before the retry
    // the staging file has gone, so it cannot be reopened. The first POST
    // reached the server, so the failure cannot claim no remote effect.
    let directory = transfer_directory("post-download-reset");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/export", listener.local_addr().unwrap());
    let server = {
        let directory = directory.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let mut entries = fs::read_dir(&directory).await.unwrap();
            while let Some(entry) = entries.next_entry().await.unwrap() {
                if entry.file_name().to_string_lossy().ends_with(".part") {
                    fs::remove_file(entry.path()).await.unwrap();
                }
            }
            let head = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 64\r\nConnection: close\r\n\r\nabc";
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        })
    };
    let result = execute(
        &transfer_engine(&directory, 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": url,
                "method": "POST",
                "retry": {"max_attempts": 2, "retry_non_idempotent": true, "backoff_base_ms": 1}
            },
            "input": {"file": {"path": "export.bin"}}
        }),
    )
    .await;
    server.await.unwrap();
    let error = &result["errors"][0];
    assert_eq!(error["code"], "DOWNLOAD_WRITE_FAILED", "{result}");
    assert_eq!(error["phase"], "write");
    assert_eq!(error["remote_effect"], "unknown");
    assert_eq!(error["retry"]["kind"], "requires_recovery");
    let _ = fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn numeric_limits_without_an_exact_value_are_refused_before_the_network() {
    // Each value used to be clamped or saturated: a rate above 10^9 per second
    // gave a zero interval, a subnormal rate an interval of centuries, and a
    // concurrency above the semaphore maximum panicked inside Engine::new.
    // Port 9 is never contacted: validation fails first.
    let base: ExecutionRequest = serde_json::from_value(json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {"url": "http://127.0.0.1:9/", "method": "GET"}
    }))
    .unwrap();

    for rate in [2e9, 1e9 + 1.0, f64::MAX, f64::MIN_POSITIVE, 5e-324, 5e-11] {
        let mut request = base.clone();
        request.connection.requests_per_second = Some(rate);
        let result = serde_json::to_value(local_engine().execute(request).await).unwrap();
        assert_eq!(
            result["errors"][0]["code"], "INVALID_INPUT",
            "{rate}: {result}"
        );
        assert_eq!(result["metrics"]["requests"], 0, "{rate}");
    }

    for config in [
        EngineConfig {
            max_concurrent_requests: usize::MAX,
            ..EngineConfig::default()
        },
        EngineConfig {
            max_concurrent_requests: tokio::sync::Semaphore::MAX_PERMITS + 1,
            ..EngineConfig::default()
        },
        EngineConfig {
            requests_per_second: Some(1_000_000_001),
            ..EngineConfig::default()
        },
        EngineConfig {
            requests_per_second: Some(u32::MAX),
            ..EngineConfig::default()
        },
    ] {
        // Engine::new stays infallible: it must not panic, and every
        // execution refuses the setting.
        let engine = Engine::new(EngineConfig {
            allow_private_networks: true,
            ..config
        });
        let result = serde_json::to_value(engine.execute(base.clone()).await).unwrap();
        assert_eq!(result["errors"][0]["code"], "INVALID_INPUT", "{result}");
        assert_eq!(result["metrics"]["requests"], 0);
    }

    // The boundaries themselves are accepted.
    for config in [
        EngineConfig {
            max_concurrent_requests: tokio::sync::Semaphore::MAX_PERMITS,
            ..EngineConfig::default()
        },
        EngineConfig {
            requests_per_second: Some(1_000_000_000),
            ..EngineConfig::default()
        },
    ] {
        let (url, server) = server(vec![(200, r#"{"ok":true}"#)]).await;
        let engine = Engine::new(EngineConfig {
            allow_private_networks: true,
            ..config
        });
        let result = execute(
            &engine,
            json!({
                "schema_version": 1,
                "operation": "test",
                "connection": {"url": url, "method": "GET", "requests_per_second": 1e9}
            }),
        )
        .await;
        server.await.unwrap();
        assert_eq!(result["status"], "success", "{result}");
    }
}

#[tokio::test]
async fn a_zero_backoff_base_never_waits_whatever_the_factor() {
    // base 0 with factor f64::MAX: the old formula computed 0 · inf = NaN on
    // the second retry and waited max_backoff_ms. The cap here is ten minutes,
    // so the old behaviour cannot finish inside the guard below; the exact
    // backoff waits zero on every retry.
    let (url, server) = server(vec![
        (503, r#"{"error":"busy"}"#),
        (503, r#"{"error":"busy"}"#),
        (503, r#"{"error":"busy"}"#),
        (200, r#"{"ok":true}"#),
    ])
    .await;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "retry": {
                "max_attempts": 4,
                "backoff_base_ms": 0,
                "backoff_factor": f64::MAX,
                "max_backoff_ms": 600_000,
                "retry_on_status": [503]
            }
        }
    });

    let result = timeout(Duration::from_secs(120), execute(&local_engine(), request))
        .await
        .expect("a zero backoff base must not wait max_backoff_ms");
    server.await.unwrap();

    assert_eq!(result["status"], "success", "{result}");
    assert_eq!(result["metrics"]["requests"], 4);
    assert_eq!(result["metrics"]["retries"], 3);
}

/// ERR-014: the axes of an error raised once a request of the operation went
/// out. The remote effect and the retry change; the phase is the one that had
/// started (ERR-003), and the category and the code still say what failed.
fn assert_after_sent_request(error: &Value, code: &str, phase: &str, result: &Value) {
    assert_eq!(error["code"], code, "{result}");
    assert_eq!(error["phase"], phase, "{result}");
    assert_eq!(error["remote_effect"], "unknown", "{result}");
    assert_eq!(error["retry"]["kind"], "requires_recovery", "{result}");
}

/// The axes of a failure before any request: nothing went out.
fn assert_nothing_sent(error: &Value, code: &str, phase: &str, result: &Value) {
    assert_eq!(error["code"], code, "{result}");
    assert_eq!(error["phase"], phase, "{result}");
    assert_eq!(error["remote_effect"], "none", "{result}");
    assert_eq!(error["retry"]["kind"], "never", "{result}");
}

#[tokio::test]
async fn a_cross_origin_redirect_after_a_post_does_not_claim_no_remote_effect() {
    // The POST and its body reach the server, which answers with a redirect
    // to another origin; the engine refuses to follow it. The server may have
    // acted on the POST: a 3xx is not proof that it did not.
    let (url, server, observed) = recorded_server(vec![(
        307,
        "",
        vec![("Location", "http://localhost:9/elsewhere")],
    )])
    .await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "POST",
                "parameters": [{"name": "a", "mode": "fixed", "value": 1, "location": "body"}],
                "request": {"allow_redirects": true, "max_redirects": 3, "body_type": "json"}
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(observed.lock().unwrap().len(), 1);
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["metrics"]["requests"], 1, "{result}");
    let error = &result["errors"][0];
    assert_eq!(error["category"], "authorization", "{result}");
    assert_after_sent_request(error, "UNSAFE_ADDRESS", "read", &result);
}

#[tokio::test]
async fn a_cross_origin_poll_after_an_accepted_submit_does_not_claim_no_remote_effect() {
    // The job was accepted; only then is its polling URL refused.
    let (url, server, observed) = recorded_server(vec![(
        202,
        r#"{"status":"pending","poll":"http://localhost:9/jobs/1"}"#,
        vec![],
    )])
    .await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "POST",
                "polling": {
                    "url_path": "poll",
                    "status_path": "status",
                    "interval_ms": 0,
                    "max_attempts": 3
                }
            }
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(observed.lock().unwrap().len(), 1);
    assert_eq!(result["status"], "failed", "{result}");
    assert_after_sent_request(&result["errors"][0], "UNSAFE_ADDRESS", "read", &result);
}

#[tokio::test]
async fn a_source_file_lost_before_a_retry_does_not_claim_no_remote_effect() {
    // The first attempt of the upload is sent; the server removes the source
    // file before answering 503, so the second attempt cannot reopen it. The
    // local failure follows a request that went out.
    let directory = transfer_directory("retry-source");
    let source = directory.join("payload.bin");
    fs::write(&source, b"payload").await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let removed = source.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = timeout(Duration::from_secs(30), listener.accept())
            .await
            .expect("the first attempt")
            .unwrap();
        let request = read_request(&mut stream).await;
        std::fs::remove_file(&removed).unwrap();
        stream
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        request
    });
    let result = execute(
        &transfer_engine(&directory, 1024, 1024),
        json!({
            "schema_version": 1,
            "operation": "upload",
            "connection": {
                "url": format!("http://{address}/"),
                "method": "PUT",
                "request": {"body_type": "raw"},
                "retry": {"max_attempts": 2, "backoff_base_ms": 0}
            },
            "input": {"file": {"path": "payload.bin"}}
        }),
    )
    .await;
    let first = server.await.unwrap();
    assert!(
        first.ends_with("payload"),
        "the first attempt carried the body"
    );
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["metrics"]["requests"], 1, "{result}");
    let error = &result["errors"][0];
    assert_eq!(error["category"], "io", "{result}");
    // The failure is the local reopening of the source: phase write.
    assert_after_sent_request(error, "FILE_IO", "write", &result);
    fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn a_failed_token_request_does_not_claim_no_remote_effect() {
    // The OAuth token request is a request of the operation too.
    let (base_url, server, _) =
        recorded_server(vec![(400, r#"{"error":"invalid_client"}"#, vec![])]).await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": format!("{base_url}resource"),
                "method": "GET",
                "auth": {
                    "type": "oauth2_client_credentials",
                    "token_url": format!("{base_url}token"),
                    "client_id": "client",
                    "client_secret": "secret"
                }
            }
        }),
    )
    .await;
    server.await.unwrap();
    let error = &result["errors"][0];
    assert_eq!(error["category"], "authentication", "{result}");
    // Obtaining the token is the connect phase.
    assert_after_sent_request(error, "AUTHENTICATION_FAILED", "connect", &result);
}

#[tokio::test]
async fn enrich_judges_the_remote_effect_on_the_requests_of_each_record() {
    // Record 0 is sent and redirected to another origin; record 1 lacks its
    // parameter and is never sent, although record 0 was. Sequential and
    // concurrent enrichment judge each record the same way.
    for concurrency in [1, 2] {
        let (url, server, observed) = recorded_server(vec![(
            307,
            "",
            vec![("Location", "http://localhost:9/elsewhere")],
        )])
        .await;
        let result = execute(
            &local_engine(),
            json!({
                "schema_version": 1,
                "operation": "enrich",
                "connection": {
                    "url": url,
                    "method": "GET",
                    "parameters": [{
                        "name": "id",
                        "mode": "mapped",
                        "source": "id",
                        "required": true,
                        "location": "query"
                    }],
                    "request": {"allow_redirects": true, "max_redirects": 3}
                },
                "input": {"records": [{"id": "a"}, {"other": "b"}]},
                "options": {"continue_on_error": true, "enrichment_concurrency": concurrency}
            }),
        )
        .await;
        server.await.unwrap();
        assert_eq!(observed.lock().unwrap().len(), 1, "{result}");
        let errors = result["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 2, "{result}");
        assert_eq!(errors[0]["input_index"], 0, "{result}");
        assert_after_sent_request(&errors[0], "UNSAFE_ADDRESS", "read", &result);
        assert_eq!(errors[1]["input_index"], 1, "{result}");
        assert_eq!(errors[1]["remote_effect"], "none", "{result}");
        assert_eq!(errors[1]["retry"]["kind"], "never", "{result}");
    }
}

#[tokio::test]
async fn failures_before_any_request_keep_no_remote_effect() {
    // Loopback is refused by default before the request is sent.
    let blocked = execute(
        &Engine::new(EngineConfig::default()),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {"url": "http://127.0.0.1:9/", "method": "POST"}
        }),
    )
    .await;
    assert_eq!(blocked["metrics"]["requests"], 0, "{blocked}");
    assert_nothing_sent(
        &blocked["errors"][0],
        "UNSAFE_ADDRESS",
        "validate",
        &blocked,
    );

    // A missing parameter fails while the request is prepared.
    let missing = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": "http://127.0.0.1:9/",
                "method": "GET",
                "parameters": [{
                    "name": "id",
                    "mode": "mapped",
                    "source": "id",
                    "required": true,
                    "location": "query"
                }]
            }
        }),
    )
    .await;
    assert_eq!(missing["metrics"]["requests"], 0, "{missing}");
    assert_eq!(missing["errors"][0]["remote_effect"], "none", "{missing}");
    assert_eq!(missing["errors"][0]["retry"]["kind"], "never", "{missing}");
    assert_ne!(missing["errors"][0]["phase"], "read", "{missing}");
}

#[tokio::test]
async fn batch_enrich_judges_the_remote_effect_on_the_records_it_sent() {
    // Record 0 goes out in the batch request, which is redirected to another
    // origin; record 1 fails its parameter and is left out of that request.
    let (url, server, observed) = recorded_server(vec![(
        307,
        "",
        vec![("Location", "http://localhost:9/elsewhere")],
    )])
    .await;
    let result = execute(
        &local_engine(),
        json!({
            "schema_version": 1,
            "operation": "enrich",
            "connection": {
                "url": url,
                "method": "POST",
                "parameters": [{
                    "name": "id",
                    "mode": "mapped",
                    "source": "id",
                    "required": true,
                    "location": "body"
                }],
                "request": {"body_type": "json", "allow_redirects": true, "max_redirects": 3},
                "batch": {
                    "enabled": true,
                    "max_size": 2,
                    "input_key": "items",
                    "input_format": "array",
                    "output_path": "results"
                }
            },
            "input": {"records": [{"id": 1}, {"other": 2}]},
            "options": {"continue_on_error": true}
        }),
    )
    .await;
    server.await.unwrap();
    assert_eq!(observed.lock().unwrap().len(), 1, "{result}");
    let errors = result["errors"].as_array().unwrap();
    let error_of = |index: u64| {
        errors
            .iter()
            .find(|error| error["input_index"] == index)
            .unwrap_or_else(|| panic!("no error for record {index}: {result}"))
    };
    assert_after_sent_request(error_of(0), "UNSAFE_ADDRESS", "read", &result);
    assert_eq!(error_of(1)["remote_effect"], "none", "{result}");
    assert_eq!(error_of(1)["retry"]["kind"], "never", "{result}");
}
