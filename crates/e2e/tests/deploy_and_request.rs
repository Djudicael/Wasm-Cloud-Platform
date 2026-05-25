/// End-to-end test: Deploy hello-axum.wasm and send HTTP request
///
/// This test:
/// 1. Starts a NATS container
/// 2. Starts a wasm-node process
/// 3. Uploads hello-axum.wasm to the artifact server
/// 4. Publishes a DeployApp event
/// 5. Adds a route
/// 6. Sends an HTTP request and verifies the response
///
/// To run:
/// ```
/// # Build prerequisites
/// cargo build --bin wasm-node
/// cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release
///
/// # Run test
/// cargo test -p e2e test_deploy_and_serve_http -- --ignored --nocapture
/// ```
mod harness;

use messaging::events::Event;

use harness::*;

#[tokio::test]
async fn test_deploy_and_serve_http() {
    let nats_port = reserve_test_port().expect("Failed to reserve NATS port");
    let proxy_port = reserve_test_port().expect("Failed to reserve proxy port");
    let artifact_port = reserve_test_port().expect("Failed to reserve artifact port");
    let admin_port = reserve_test_port().expect("Failed to reserve admin port");

    // 1. Start NATS
    let nats = NatsContainer::start(nats_port)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");

    // Set up JetStream
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    // 2. Start node
    let node = NodeProcess::start_with_admin(
        "test-node-0",
        &nats.url,
        proxy_port,
        artifact_port,
        admin_port,
    )
    .await
    .expect("Failed to start node");

    // 3. Upload the Wasm artifact to the node artifact server and authorize it for this node
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let (artifact_url, sha256, size_bytes, manifests) =
        upload_and_authorize_artifact_for_node(&node, &wasm_path)
            .await
            .expect("Failed to prepare artifact on node artifact server");

    // 4. Deploy the app
    let app_id = "hello-axum:v1";
    eprintln!("Deploying with artifact URL: {}", artifact_url);

    let config = build_app_config(app_id, 100_000_000, 100, 2);

    eprintln!("Deploying app: {}", app_id);
    bus.publish(&Event::DeployApp {
        app_id: common::types::AppId(app_id.to_string()),
        config,
        artifact_url,
        artifact_transfer_manifests: manifests,
        expected_hash: Some(sha256.clone()),
        size_bytes,
    })
    .await
    .expect("Failed to deploy app");

    // 5. Add route
    eprintln!("Adding route: test-app.local -> {}", app_id);
    add_route(&bus, "test-app.local", app_id)
        .await
        .expect("Failed to add route");

    // 6. Wait for app to be ready (cold start compilation)
    eprintln!("Waiting for app to be ready (cold start)...");
    wait_for_app_ready(node.proxy_port, "test-app.local", 30)
        .await
        .expect("App did not become ready");

    // 7. Send HTTP request
    eprintln!("Sending HTTP request to /");
    let response = send_request(node.proxy_port, "test-app.local", "/")
        .await
        .expect("Failed to send request");

    // 8. Verify response
    assert_eq!(
        response.status(),
        200,
        "Expected 200 OK, got {}",
        response.status()
    );

    let body = response.text().await.expect("Failed to read response body");
    assert!(
        body.contains("Hello"),
        "Expected response to contain 'Hello', got: {}",
        body
    );

    eprintln!("✓ Response: {}", body);

    // 9. Test another endpoint
    eprintln!("Sending HTTP request to /health");
    let health_response = send_request(node.proxy_port, "test-app.local", "/health")
        .await
        .expect("Failed to send health request");

    assert_eq!(health_response.status(), 200);

    // Cleanup
    node.stop().ok();

    eprintln!("✅ test_deploy_and_serve_http PASSED");
}

#[test]
fn test_e2e_infrastructure() {
    // Verify dependencies are available
    assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).exists());
}
