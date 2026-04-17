/// East-West Traffic Tests
///
/// Tests for app-to-app communication (service discovery)
mod harness;

use harness::*;

#[tokio::test]
#[ignore]
async fn test_east_west_traffic() {
    // 1. Start NATS
    let nats = NatsContainer::start(4224)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    // 2. Start node
    let node = NodeProcess::start("test-node-east-west", &nats.url, 8182, 9002)
        .await
        .expect("Failed to start node");

    // Wait for node to be ready
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 3. Deploy echo-service app
    let echo_wasm_path = find_echo_service_wasm().expect("echo-service.wasm not found");
    let echo_sha256 = compute_sha256(&echo_wasm_path).expect("Failed to compute SHA-256");
    let echo_size_bytes = std::fs::metadata(&echo_wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &echo_wasm_path, &echo_sha256)
        .await
        .expect("Failed to upload echo-service artifact");

    let echo_app_id = "echo-service:v1";
    let echo_artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, echo_sha256
    );

    // Configure echo-service to bind on port 8081
    let mut echo_config = build_app_config(echo_app_id, 100_000_000, 100, 1);
    echo_config.wasm_bind_port = 8081;

    deploy_app(
        &bus,
        echo_app_id,
        echo_artifact_url,
        echo_sha256,
        echo_size_bytes,
        echo_config,
    )
    .await
    .expect("Failed to deploy echo-service");

    // Add route for echo-service
    add_route(&bus, "echo.local", echo_app_id)
        .await
        .expect("Failed to add echo route");

    // Wait for echo-service to be ready
    wait_for_app_ready(node.proxy_port, "echo.local", 30)
        .await
        .expect("echo-service did not become ready");

    eprintln!("✓ echo-service deployed and ready");

    // 4. Deploy hello-axum app
    let hello_wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let hello_sha256 = compute_sha256(&hello_wasm_path).expect("Failed to compute SHA-256");
    let hello_size_bytes = std::fs::metadata(&hello_wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &hello_wasm_path, &hello_sha256)
        .await
        .expect("Failed to upload hello-axum artifact");

    let hello_app_id = "hello-axum:v1";
    let hello_artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, hello_sha256
    );

    // Configure hello-axum to call echo-service via service discovery
    // The platform will inject ECHO_SERVICE_URL pointing to echo-service's address
    let mut hello_config = build_app_config(hello_app_id, 100_000_000, 100, 1);
    hello_config.wasm_bind_port = 8080;
    // Don't set ECHO_SERVICE_URL - let platform inject it via service discovery

    deploy_app(
        &bus,
        hello_app_id,
        hello_artifact_url,
        hello_sha256,
        hello_size_bytes,
        hello_config,
    )
    .await
    .expect("Failed to deploy hello-axum");

    // Add route for hello-axum
    add_route(&bus, "hello.local", hello_app_id)
        .await
        .expect("Failed to add hello route");

    // Wait for hello-axum to be ready
    wait_for_app_ready(node.proxy_port, "hello.local", 30)
        .await
        .expect("hello-axum did not become ready");

    eprintln!("✓ hello-axum deployed and ready");

    // 5. Test that hello-axum can serve requests (basic functionality)
    eprintln!("Testing basic functionality: hello-axum should return 200 on /");
    let response = send_request(node.proxy_port, "hello.local", "/")
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        200,
        "Expected 200, got {}",
        response.status()
    );

    let body = response.text().await.unwrap();
    eprintln!("Response from /: {}", body);

    // 6. Test that echo-service can serve requests (basic functionality)
    eprintln!("Testing basic functionality: echo-service should return 200 on /");
    let response = send_request(node.proxy_port, "echo.local", "/")
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        200,
        "Expected 200, got {}",
        response.status()
    );

    let body = response.text().await.unwrap();
    eprintln!("Response from echo-service /: {}", body);

    // 7. Test East-West traffic: hello-axum calls echo-service via /call-echo
    // This verifies service discovery works - platform injects ECHO_SERVICE_URL
    eprintln!("Testing East-West traffic: hello-axum -> /call-echo -> echo-service");
    let response = send_request(node.proxy_port, "hello.local", "/call-echo")
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        200,
        "Expected 200, got {}",
        response.status()
    );

    let body = response.text().await.unwrap();
    eprintln!("Response from /call-echo: {}", body);

    // Verify the response contains the expected echo message
    assert!(
        body.contains("Echo from echo-service"),
        "Expected response to contain 'Echo from echo-service', got: {}",
        body
    );

    eprintln!("✅ test_east_west_traffic PASSED - Direct app-to-app communication works!");

    node.stop().ok();
}