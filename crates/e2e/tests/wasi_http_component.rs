/// End-to-end test: Deploy a wasi:http component and serve it through the proxy.
mod harness;

use common::artifact_transfer::{ArtifactManifestBatchRequest, ArtifactManifestBatchResponse};
use messaging::events::Event;

use harness::*;

#[tokio::test]
#[ignore = "requires NATS + wasm-node binary + compiled http-hello-component.wasm"]
async fn test_deploy_and_serve_wasi_http_component() {
    let nats = NatsContainer::start(4322)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start("wasi-http-node-0", &nats.url, 8280, 9101)
        .await
        .expect("Failed to start node");

    let wasm_path = find_http_hello_component_wasm().expect("http-hello-component.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path)
        .expect("Failed to get file size")
        .len();
    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact to node artifact server");
    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    let authorize_url = format!(
        "http://127.0.0.1:{}/artifacts/{}/authorize",
        node.artifact_port, sha256
    );
    let authorize_response = reqwest::Client::new()
        .post(&authorize_url)
        .json(&ArtifactManifestBatchRequest {
            audiences: vec![node.node_id.clone()],
        })
        .send()
        .await
        .expect("Failed to authorize artifact manifests");
    assert!(
        authorize_response.status().is_success(),
        "artifact authorize failed with status {}",
        authorize_response.status()
    );
    let manifests = authorize_response
        .json::<ArtifactManifestBatchResponse>()
        .await
        .expect("Failed to parse artifact manifest batch response")
        .manifests;

    let app_id = "http-hello-component:v1";
    let config = build_app_config(app_id, 100_000_000, 100, 2);

    bus.publish(&Event::DeployApp {
        app_id: common::types::AppId(app_id.to_string()),
        config,
        artifact_url,
        artifact_transfer_manifests: manifests,
        expected_hash: Some(sha256),
        size_bytes,
    })
    .await
    .expect("Failed to publish DeployApp event");

    add_route(&bus, "wasi-http.local", app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "wasi-http.local", 30)
        .await
        .expect("wasi:http component did not become ready");

    let response = send_request(node.proxy_port, "wasi-http.local", "/")
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("Failed to read response body");
    assert!(
        body.contains("Hello from wasi:http!"),
        "unexpected response body: {body}"
    );

    let health_response = send_request(node.proxy_port, "wasi-http.local", "/health")
        .await
        .expect("Failed to send /health request");
    assert_eq!(health_response.status(), 200);
    let health_body = health_response
        .text()
        .await
        .expect("Failed to read /health response body");
    assert!(
        health_body.contains("healthy"),
        "unexpected /health response body: {health_body}"
    );
}
