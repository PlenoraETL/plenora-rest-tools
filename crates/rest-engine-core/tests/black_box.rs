use rest_engine_core::{Engine, EngineConfig};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
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
    assert_eq!(result["errors"][0]["code"], "policy_violation");
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
    assert_eq!(escaped["errors"][0]["code"], "policy_violation");

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
    assert_eq!(existing["errors"][0]["code"], "file_io");
    assert_eq!(fs::read(&destination).await.unwrap(), b"keep-me");
    fs::remove_dir_all(directory).await.unwrap();
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
    assert_eq!(result["output"]["sha256"], sha256(&body));
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
    assert_eq!(result["output"]["sha256"], sha256(&body));
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
    assert_eq!(result["errors"][0]["code"], "unsafe_address");
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
    assert_eq!(limited["errors"][0]["code"], "file_too_large");
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
    assert_eq!(checksum["errors"][0]["code"], "checksum_mismatch");
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
    assert_eq!(result["output"]["sha256"], sha256(&body));
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
        "http_status"
    );
    assert_eq!(
        execute(&engine, request.clone()).await["errors"][0]["code"],
        "http_status"
    );
    let rejected = execute(&engine, request).await;
    server.await.unwrap();

    assert_eq!(rejected["errors"][0]["code"], "circuit_open");
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

async fn owned_recorded_server(
    responses: Vec<OwnedTestResponse>,
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
use std::{
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
