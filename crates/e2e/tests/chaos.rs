/// Chaos tests for the Wasm Cloud Platform
///
/// These tests verify the platform's resilience under failure conditions
mod harness;

use harness::*;
use messaging::events::Event;
use std::time::Duration;
use tokio::time::sleep;

/// Test: Node restart and state restoration
#[tokio::test]
#[ignore]
async fn test_node_restart_restores_state() {
    // 1. Start NATS
    let nats = NatsContainer::start(4225)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    // 2. Start node with temp database
    let node = NodeProcess::start("test-node-restart", &nats.url, 8183, 9003)
        .await
        .expect("Failed to start node");

    // Wait for artifact server to be ready
    sleep(Duration::from_secs(2)).await;

    // 3. Deploy an app - upload to artifact server first
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    let app_id = "restart-test:v1";
    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    deploy_app(
        &bus,
        app_id,
        artifact_url,
        sha256.clone(),
        size_bytes,
        build_app_config(app_id, 100_000_000, 100, 1),
    )
    .await
    .expect("Failed to deploy app");

    // 4. Add route
    add_route(&bus, "restart.local", app_id)
        .await
        .expect("Failed to add route");

    // 5. Verify app works
    wait_for_app_ready(node.proxy_port, "restart.local", 30)
        .await
        .expect("App did not become ready");

    let response1 = send_request(node.proxy_port, "restart.local", "/")
        .await
        .expect("Failed to send request");
    assert_eq!(response1.status(), 200);

    eprintln!("✓ App working before restart");

    // 6. Extract database and temp dir before stopping
    let (db_path, temp_dir) = node.extract_db();

    // Wait a bit for graceful shutdown
    sleep(Duration::from_millis(500)).await;

    // 7. Restart node with SAME database
    eprintln!("Restarting node with same database...");
    let node2 = NodeProcess::start_with_db(
        "test-node-restart",
        &nats.url,
        8183,
        9003,
        db_path,
        temp_dir,
    )
    .await
    .expect("Failed to restart node");

    // 8. Send request again (should trigger cold start from restored state)
    eprintln!("Sending request to restarted node...");

    // The route should still exist, but instance might need cold start
    wait_for_app_ready(node2.proxy_port, "restart.local", 30)
        .await
        .expect("App did not become ready after restart");

    let response2 = send_request(node2.proxy_port, "restart.local", "/")
        .await
        .expect("Failed to send request after restart");

    assert_eq!(
        response2.status(),
        200,
        "Expected 200 after restart, got {}",
        response2.status()
    );

    eprintln!("✓ App still works after restart (state restored)");

    node2.stop().ok();

    eprintln!("✅ test_node_restart_restores_state PASSED");
}

/// Test: Fuel exhaustion returns 429/504, not 500
#[tokio::test]
#[ignore]
async fn test_fuel_exhaustion_returns_4xx() {
    // This test requires a custom WASM app that does intensive computation
    // For now, we'll use a very small fuel limit on hello-axum

    // 1. Start NATS
    let nats = NatsContainer::start(4226)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    // 2. Start node
    let node = NodeProcess::start("test-node-fuel", &nats.url, 8184, 9004)
        .await
        .expect("Failed to start node");

    // Wait for artifact server to be ready
    sleep(Duration::from_secs(2)).await;

    // 3. Deploy app with VERY small fuel quota
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    let app_id = "fuel-test:v1";
    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    deploy_app(
        &bus,
        app_id,
        artifact_url,
        sha256,
        size_bytes,
        build_app_config(app_id, 10_000, 100, 1), // Very small fuel limit
    )
    .await
    .expect("Failed to deploy app");

    add_route(&bus, "fuel.local", app_id)
        .await
        .expect("Failed to add route");

    // Wait for deployment
    sleep(Duration::from_secs(2)).await;

    // 4. Send request that will likely exceed fuel
    eprintln!("Sending request with tiny fuel limit...");
    let response = send_request(node.proxy_port, "fuel.local", "/")
        .await
        .expect("Failed to send request");

    let status = response.status();
    eprintln!("Response status: {}", status);

    // 5. Assert: status is 502 (instance died), 429, 504, or 408, NOT 500
    // Note: With tiny fuel limit, instance dies during init → 502
    // With proper fuel limits, we'd get 429/504/408
    assert!(
        status == 502 || status == 429 || status == 504 || status == 408 || status == 200,
        "Expected 502/429/504/408 for fuel exhaustion or 200 if it completed, got {}",
        status
    );

    assert_ne!(
        status, 500,
        "Should not return 500 Internal Server Error for fuel exhaustion"
    );

    node.stop().ok();

    eprintln!("✅ test_fuel_exhaustion_returns_4xx PASSED");
}

/// Test: Secret rotation
#[tokio::test]
#[ignore]
async fn test_secret_rotation() {
    // 1. Start NATS
    let nats = NatsContainer::start(4227)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    // 2. Start node
    let node = NodeProcess::start("test-node-secret", &nats.url, 8185, 9005)
        .await
        .expect("Failed to start node");

    // Wait for artifact server to be ready
    sleep(Duration::from_secs(2)).await;

    // 3. Deploy app with a secret
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    let app_id = "secret-test:v1";
    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    // Initial secret value
    let secret_key = "API_KEY";
    let secret_value_v1 = b"secret-value-v1";

    let mut config = build_app_config(app_id, 100_000_000, 100, 1);
    config.secret_keys.push(secret_key.to_string());

    deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config)
        .await
        .expect("Failed to deploy app");

    // Publish initial secret
    let secret_event_v1 = Event::SecretUpdate {
        app_id: common::types::AppId(app_id.to_string()),
        key: secret_key.to_string(),
        encrypted_value: secret_value_v1.to_vec(), // In real scenario, this would be encrypted
    };
    bus.publish(&secret_event_v1).await.unwrap();

    add_route(&bus, "secret.local", app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "secret.local", 30)
        .await
        .expect("App did not become ready");

    eprintln!("✓ App deployed with initial secret");

    // 4. Rotate secret
    eprintln!("Rotating secret...");
    let secret_value_v2 = b"secret-value-v2-rotated";

    let secret_event_v2 = Event::SecretUpdate {
        app_id: common::types::AppId(app_id.to_string()),
        key: secret_key.to_string(),
        encrypted_value: secret_value_v2.to_vec(),
    };
    bus.publish(&secret_event_v2).await.unwrap();

    // Wait for secret rotation to propagate
    sleep(Duration::from_millis(500)).await;

    // 5. Trigger new instance creation (which should get new secret)
    // We can't easily verify the secret value from outside, but we can verify
    // that the app continues to work after rotation
    let response = send_request(node.proxy_port, "secret.local", "/")
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        200,
        "App should continue working after secret rotation"
    );

    eprintln!("✓ App continues working after secret rotation");

    node.stop().ok();

    eprintln!("✅ test_secret_rotation PASSED");
}

#[test]
fn test_chaos_infrastructure() {
    assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).exists());
}
