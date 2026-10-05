use plenora_rest_core::{
    CancellationToken, Engine, EngineConfig, ExecutionControl, ExecutionRequest,
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
            format!(r#"{{"status":"completed","items":[{{"id":2}}]}}"#),
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
                "cookies": {"enabled": true, "jar_id": "session"},
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
    // would refuse every later `jar_id` for the rest of its life.
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        max_pooled_origins: 300,
        ..EngineConfig::default()
    });
    // Well past the 256 jar bound, each with its own session.
    for index in 0..300 {
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
                    "cookies": {"enabled": true, "jar_id": format!("tenant-{index}")}
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
    let session = |url: &str, jar: &str| {
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
                "cookies": {"enabled": true, "jar_id": jar}
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
    let holder = tokio::spawn(async move {
        execute(&holder_engine, session(&stalled_url, "tenant-oldest")).await
    });
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
        let result = execute(&engine, session(&url, &format!("tenant-{index}"))).await;
        assert_eq!(
            result["status"], "success",
            "session {index} must be admitted while the oldest jar is busy: {result}"
        );
        server.await.unwrap();
    }

    holder.abort();
    stalled_server.abort();
}

/// One past the engine's jar bound, so the loop above forces an eviction.
const MAX_COOKIE_JARS_IN_TEST: usize = 257;

#[tokio::test]
async fn a_jar_reserved_by_a_running_request_is_not_evicted() {
    // Evicting a jar that a request is still using would split one session in
    // two: the running request stores its cookies in the jar it is holding,
    // while the next request naming the same `jar_id` is handed a fresh one.
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
    let session = |url: &str, jar: &str| {
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                // Far longer than the loop below can take, so the stuck request
                // cannot time out and release its jar on its own.
                "request": {"timeout_ms": 600_000},
                "cookies": {"enabled": true, "jar_id": jar}
            }
        })
    };

    let held_engine = Arc::clone(&engine);
    let held_target = held_url.clone();
    let held =
        tokio::spawn(async move { execute(&held_engine, session(&held_target, "held")).await });
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
        let result = execute(&engine, session(&url, &format!("tenant-{index}"))).await;
        assert_eq!(
            result["status"], "success",
            "session {index} must be admitted: {result}"
        );
        filler.await.unwrap();
    }

    release.send(()).unwrap();
    let first = held.await.unwrap();
    assert_eq!(first["status"], "success", "{first}");
    let second = execute(&engine, session(&held_url, "held")).await;
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
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "cookies": {"enabled": true, "jar_id": "bounded"}
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
    let with_jar = |url: &str| {
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "cookies": {"enabled": true, "jar_id": "session"}
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
                        // A product beyond u64 is not representable as an exact
                        // JSON integer, so it must be null and not a lossy float.
                        {"source": "big", "column": "squared", "operation": "multiply", "value": u64::MAX},
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
    assert_eq!(record["squared"], Value::Null);
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
            "cookies": {"enabled": true, "jar_id": "default"}
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
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "cookies": {"enabled": true, "jar_id": "tenant-a"}
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

#[tokio::test]
async fn a_request_on_an_evicted_cookie_jar_fails_instead_of_losing_the_session() {
    // Past the jar bound the oldest idle jar is evicted, and its cookies with
    // it. Coming back with the same `jar_id` must not start from an empty jar,
    // which would log the caller out without a signal.
    let engine = Engine::new(EngineConfig {
        allow_private_networks: true,
        allow_cookie_store: true,
        max_pooled_origins: 0,
        ..EngineConfig::default()
    });
    let session = |url: &str, jar: &str| {
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": url,
                "method": "GET",
                "cookies": {"enabled": true, "jar_id": jar}
            }
        })
    };
    for index in 0..MAX_COOKIE_JARS_IN_TEST {
        let (url, server, _) = recorded_server(vec![(
            200,
            r#"{"ok":true}"#,
            vec![("Set-Cookie", "sid=login; Path=/")],
        )])
        .await;
        let result = execute(&engine, session(&url, &format!("tenant-{index}"))).await;
        server.await.unwrap();
        assert_eq!(result["status"], "success", "session {index}: {result}");
    }

    // `tenant-0` was the oldest idle jar, so it is the one that went.
    let returning = execute(&engine, session("http://127.0.0.1:9/", "tenant-0")).await;
    assert_eq!(returning["status"], "failed", "{returning}");
    assert_eq!(returning["errors"][0]["code"], "POLICY_VIOLATION");
    assert_eq!(returning["metrics"]["requests"], 0);

    let oversized = execute(&engine, session("http://127.0.0.1:9/", &"j".repeat(257))).await;
    assert_eq!(
        oversized["errors"][0]["code"], "INVALID_INPUT",
        "{oversized}"
    );
}
