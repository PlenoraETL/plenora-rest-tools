use super::*;

#[tokio::test]
async fn frozen_remote_body_and_sensitive_header_matrix() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/frozen/0.4.0/remote-private.json"
    ))
    .unwrap();
    for case in fixture["bodies"].as_array().unwrap() {
        for status in [200, 400] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let body = case["body"].as_str().unwrap().to_owned();
            let headers = fixture["headers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| format!("{}: PRIVATE_HEADER_SENTINEL\r\n", name.as_str().unwrap()))
                .collect::<String>();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(request.contains("Bearer PRIVATE_CREDENTIAL_SENTINEL"));
                let reply = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            });
            let result = execute(&local_engine(), json!({"schema_version":1,"operation":"test",
                "connection":{"url":format!("{origin}/PRIVATE_URL_SENTINEL"),"auth":{"type":"bearer","token":"PRIVATE_CREDENTIAL_SENTINEL"},
                    "response":{"format":case["format"],"delimiter":",","success_when":case["success_when"],"error_path":case["error_path"]}},
                "options":{"capture_response_metadata":true,"response_headers":["*"]}})).await;
            server.await.unwrap();
            if case["id"] == "headers" && status == 200 {
                assert_eq!(result["status"], "success", "{result}");
                assert_eq!(result["responses"].as_array().unwrap().len(), 1);
            } else {
                assert_eq!(result["status"], "failed", "{case}: {result}");
                assert_eq!(
                    result["errors"][0]["code"],
                    if status == 400 {
                        json!("HTTP_STATUS")
                    } else {
                        case["code"].clone()
                    },
                    "{case}: {result}"
                );
            }
            assert!(
                !result.to_string().contains("PRIVATE_"),
                "assertion failed: private output: {result}"
            );
            use base64::Engine as _;
            assert!(!result.to_string().contains(
                &base64::engine::general_purpose::STANDARD.encode("PRIVATE_REMOTE_SENTINEL")
            ));
            let mut normalized = result;
            for response in normalized["responses"].as_array_mut().unwrap() {
                assert_eq!(response["final_url"], origin);
                response["final_url"] = json!("<loopback-origin>");
            }
            snapshot::observe(
                &format!("remote-{}-{status}", case["id"].as_str().unwrap()),
                normalized,
            );
        }
    }
}
