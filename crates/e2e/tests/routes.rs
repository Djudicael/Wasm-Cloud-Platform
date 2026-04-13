/// Route management tests
///
/// Tests for adding/removing routes and verifying traffic routing

mod harness;

use harness::*;
use messaging::events::Event;

#[tokio::test]
#[ignore]
async fn test_route_add_and_serve() {
    // 1. Start NATS
    let nats = NatsContainer::start(4223)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    // 2. Start node
    let node = NodeProcess::start("test-node-routes", &nats.url, 8181, 9001)
        .await
        .expect("Failed to start node");

    // 3. Deploy app
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    let app_id = "route-test:v1";
    let artifact_url = format!("http://127.0.0.1:{}/artifacts/{}", node.artifact_port, sha256);

    deploy_app(
        &bus,
        app_id,
        artifact_url,
        sha256,
        size_bytes,
        build_app_config(app_id, 100_000_000, 100, 1),
    )
    .await
    .expect("Failed to deploy app");

    // 4. Before adding route, requests should fail with 502 (no route)
    eprintln!("Testing request before route is added (should fail)...");
    let response_before = send_request(node.proxy_port, "route-test.local", "/")
        .await
        .expect("Failed to send request");

    assert_eq!(
        response_before.status(),
        502,
        "Expected 502 before route exists, got {}",
        response_before.status()
    );

    // 5. Add route
    eprintln!("Adding route: route-test.local -> {}", app_id);
    add_route(&bus, "route-test.local", app_id)
        .await
        .expect("Failed to add route");

    // 6. After adding route, requests should succeed
    eprintln!("Waiting for app to be ready...");
    wait_for_app_ready(node.proxy_port, "route-test.local", 30)
        .await
        .expect("App did not become ready");

    let response_after = send_request(node.proxy_port, "route-test.local", "/")
        .await
        .expect("Failed to send request");

    assert_eq!(
        response_after.status(),
        200,
        "Expected 200 after route added, got {}",
        response_after.status()
    );

    let body = response_after.text().await.unwrap();
    assert!(body.contains("Hello"));

    // 7. Remove route
    eprintln!("Removing route...");
    let remove_event = Event::RouteRemove {
        host: "route-test.local".to_string(),
    };
    bus.publish(&remove_event).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 8. After removing route, requests should fail again
    let response_removed = send_request(node.proxy_port, "route-test.local", "/")
        .await
        .expect("Failed to send request");

    assert_eq!(
        response_removed.status(),
        502,
        "Expected 502 after route removed, got {}",
        response_removed.status()
    );

    node.stop().ok();

    eprintln!("✅ test_route_add_and_serve PASSED");
}
