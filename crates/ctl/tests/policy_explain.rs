use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn policy_explain_uses_authenticated_http_without_nats() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /admin/policy/explain?app_id=agents%2Fmodel%3Av1 "));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-read-token\r\n"));
        let body = r#"{"app_id":"agents/model:v1","effective_policy":{}}"#;
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_wasm-ctl"))
            .args([
                "--nats-url",
                "nats://127.0.0.1:1",
                "--node-api",
                &format!("http://{address}"),
                "--auth-token",
                "test-read-token",
                "policy",
                "explain",
                "agents/model:v1",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("HTTP inspection must not wait for NATS")
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["app_id"], "agents/model:v1");
    server.await.unwrap();
}
