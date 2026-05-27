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
use reqwest::StatusCode;
use sha2::Digest;
use std::time::Duration;
use tokio::time::sleep;

use harness::*;

async fn wait_for_deploy_intent_ready(deploy_port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let url = format!("http://127.0.0.1:{deploy_port}/deploy/intent");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    loop {
        match client.post(&url).json(&serde_json::json!({})).send().await {
            Ok(response) if response.status() != StatusCode::NOT_FOUND => return Ok(()),
            _ if tokio::time::Instant::now() >= deadline => {
                return Err(format!("deploy intent route did not become ready: {url}").into());
            }
            _ => sleep(Duration::from_millis(250)).await,
        }
    }
}

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

#[tokio::test]
async fn test_deploy_and_serve_http_via_oci_artifact_ref() {
    use axum::{
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        routing::get,
        Router,
    };
    use tokio::net::TcpListener;
    use tokio::time::sleep;

    #[derive(Clone)]
    struct RegistryState {
        expected_auth: String,
        index_body: String,
        matching_manifest_body: String,
        non_matching_manifest_body: String,
        matching_blob_bytes: Vec<u8>,
        matching_blob_digest: String,
        matching_manifest_digest: String,
        non_matching_manifest_digest: String,
    }

    fn normalized_oci_architecture() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        }
    }

    let deployed_app_id = "default/hello-axum-oci:v1";

    let nats_port = reserve_test_port().expect("Failed to reserve NATS port");
    let proxy_port = reserve_test_port().expect("Failed to reserve proxy port");
    let artifact_port = reserve_test_port().expect("Failed to reserve artifact port");
    let admin_port = reserve_test_port().expect("Failed to reserve admin port");

    let nats = NatsContainer::start(nats_port)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start_with_admin(
        "test-node-oci-0",
        &nats.url,
        proxy_port,
        artifact_port,
        admin_port,
    )
    .await
    .expect("Failed to start node");

    let deploy_port = reserve_test_port().expect("Failed to reserve deploy ingress port");
    let deploy_artifact_port = reserve_test_port().expect("Failed to reserve deploy artifact port");
    let deploy_ingress = DeployIngressProcess::start(
        "test-deploy-ingress-0",
        &nats.url,
        deploy_port,
        deploy_artifact_port,
    )
    .await
    .expect("Failed to start deploy ingress");
    wait_for_deploy_intent_ready(deploy_ingress.deploy_port)
        .await
        .expect("Deploy intent route did not become ready");

    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read hello-axum.wasm");
    let wasm_hash = hex::encode(sha2::Sha256::digest(&wasm_bytes));

    let matching_manifest_body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "layers": [{
            "mediaType": "application/wasm",
            "digest": format!("sha256:{wasm_hash}"),
            "size": wasm_bytes.len()
        }]
    })
    .to_string();
    let non_matching_manifest_body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "layers": [{
            "mediaType": "application/wasm",
            "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "size": 16
        }]
    })
    .to_string();
    let matching_manifest_digest =
        hex::encode(sha2::Sha256::digest(matching_manifest_body.as_bytes()));
    let non_matching_manifest_digest =
        hex::encode(sha2::Sha256::digest(non_matching_manifest_body.as_bytes()));
    let index_body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{non_matching_manifest_digest}"),
                "size": non_matching_manifest_body.len(),
                "platform": {
                    "os": "linux",
                    "architecture": "arm64"
                }
            },
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{matching_manifest_digest}"),
                "size": matching_manifest_body.len(),
                "platform": {
                    "os": std::env::consts::OS,
                    "architecture": normalized_oci_architecture()
                }
            }
        ]
    })
    .to_string();

    let registry_state = RegistryState {
        expected_auth: "Bearer registry-token".to_string(),
        index_body,
        matching_manifest_body,
        non_matching_manifest_body,
        matching_blob_bytes: wasm_bytes.clone(),
        matching_blob_digest: wasm_hash.clone(),
        matching_manifest_digest,
        non_matching_manifest_digest,
    };

    let registry = Router::new()
        .route(
            "/v2/example-org/hello-axum/manifests/{reference}",
            get(
                |State(state): State<RegistryState>,
                 Path(reference): Path<String>,
                 headers: HeaderMap| async move {
                    if headers
                        .get(reqwest::header::AUTHORIZATION.as_str())
                        .and_then(|value| value.to_str().ok())
                        != Some(state.expected_auth.as_str())
                    {
                        return (StatusCode::UNAUTHORIZED, String::new());
                    }
                    let body = if reference == "v1" {
                        state.index_body.clone()
                    } else if reference == format!("sha256:{}", state.matching_manifest_digest) {
                        state.matching_manifest_body.clone()
                    } else if reference == format!("sha256:{}", state.non_matching_manifest_digest)
                    {
                        state.non_matching_manifest_body.clone()
                    } else {
                        return (StatusCode::NOT_FOUND, String::new());
                    };
                    (StatusCode::OK, body)
                },
            ),
        )
        .route(
            "/v2/example-org/hello-axum/blobs/{digest}",
            get(
                |State(state): State<RegistryState>,
                 Path(digest): Path<String>,
                 headers: HeaderMap| async move {
                    if headers
                        .get(reqwest::header::AUTHORIZATION.as_str())
                        .and_then(|value| value.to_str().ok())
                        != Some(state.expected_auth.as_str())
                    {
                        return (StatusCode::UNAUTHORIZED, Vec::new());
                    }
                    if digest != format!("sha256:{}", state.matching_blob_digest) {
                        return (StatusCode::NOT_FOUND, Vec::new());
                    }
                    (StatusCode::OK, state.matching_blob_bytes.clone())
                },
            ),
        )
        .with_state(registry_state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind local OCI registry");
    let registry_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, registry).await.unwrap();
    });

    let node_api = format!("http://127.0.0.1:{}", node.admin_port);
    let deploy_api = format!("http://127.0.0.1:{}", deploy_ingress.deploy_port);
    run_ctl_async(
        vec![
            "secrets".to_string(),
            "set-artifact-credential".to_string(),
            "--key".to_string(),
            "ghcr-reader".to_string(),
            "--value".to_string(),
            "authorization:Bearer registry-token".to_string(),
        ],
        nats.url.clone(),
        node_api.clone(),
        deploy_api.clone(),
    )
    .await
    .expect("Failed to store artifact credential via wasm-ctl");

    sleep(Duration::from_secs(1)).await;

    run_ctl_async(
        vec![
            "deploy".to_string(),
            "--app".to_string(),
            "hello-axum-oci".to_string(),
            "--version".to_string(),
            "v1".to_string(),
            "--artifact-ref".to_string(),
            format!(
                "oci://127.0.0.1:{}/example-org/hello-axum:v1",
                registry_addr.port()
            ),
            "--artifact-credential".to_string(),
            "ghcr-reader".to_string(),
        ],
        nats.url.clone(),
        node_api.clone(),
        deploy_api.clone(),
    )
    .await
    .expect("Failed to deploy app from OCI artifact ref via wasm-ctl");

    add_route(&bus, "test-oci.local", deployed_app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "test-oci.local", 30)
        .await
        .expect("OCI-deployed app did not become ready");

    let response = send_request(node.proxy_port, "test-oci.local", "/")
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("Failed to read response body");
    assert!(body.contains("Hello"), "Unexpected response body: {}", body);

    node.stop().ok();
    deploy_ingress.stop().ok();
}
