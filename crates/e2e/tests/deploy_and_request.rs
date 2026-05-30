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

#[derive(Clone)]
struct MockOciRegistryState {
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

async fn start_mock_oci_registry(
    wasm_bytes: Vec<u8>,
) -> Result<(tokio::task::JoinHandle<()>, u16, String), Box<dyn std::error::Error>> {
    use axum::{
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        routing::get,
        Router,
    };
    use tokio::net::TcpListener;

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

    let registry_state = MockOciRegistryState {
        expected_auth: "Bearer registry-token".to_string(),
        index_body,
        matching_manifest_body,
        non_matching_manifest_body,
        matching_blob_bytes: wasm_bytes,
        matching_blob_digest: wasm_hash.clone(),
        matching_manifest_digest,
        non_matching_manifest_digest,
    };

    let registry = Router::new()
        .route(
            "/v2/example-org/hello-axum/manifests/{reference}",
            get(
                |State(state): State<MockOciRegistryState>,
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
                |State(state): State<MockOciRegistryState>,
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

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, registry).await.unwrap();
    });

    Ok((handle, port, wasm_hash))
}

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

fn read_audit_records(
    audit_path: &std::path::Path,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let contents = std::fs::read_to_string(audit_path)?;
    Ok(contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()?)
}

#[derive(Debug, serde::Deserialize)]
struct DeployIngressHealth {
    ingress_id: String,
    ha_enabled: bool,
    is_leader: bool,
    leader_ingress_id: Option<String>,
    leader_artifact_server_url: Option<String>,
}

async fn wait_for_deploy_ingress_role(
    deploy_port: u16,
    expected_is_leader: bool,
) -> Result<DeployIngressHealth, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let url = format!("http://127.0.0.1:{deploy_port}/health");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

    loop {
        match client.get(&url).send().await {
            Ok(response) if response.status() == StatusCode::OK => {
                let health = response.json::<DeployIngressHealth>().await?;
                if health.is_leader == expected_is_leader {
                    return Ok(health);
                }
            }
            _ => {}
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "deploy ingress on port {deploy_port} did not reach expected leader state {expected_is_leader}"
            )
            .into());
        }

        sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn test_deploy_ingress_auth_enforces_read_write_boundaries() {
    let nats_port = reserve_test_port().expect("Failed to reserve NATS port");
    let deploy_port = reserve_test_port().expect("Failed to reserve deploy ingress port");
    let artifact_port = reserve_test_port().expect("Failed to reserve artifact ingress port");

    let nats = NatsContainer::start(nats_port)
        .await
        .expect("Failed to start NATS");
    let deploy_ingress = DeployIngressProcess::start_with_env(
        "deploy-ingress-auth",
        &nats.url,
        deploy_port,
        artifact_port,
        &[
            ("WASM_DEPLOY_INGRESS_HA_ENABLED", "false"),
            ("WASM_DEPLOY_INGRESS_AUTH_ENABLED", "true"),
            (
                "WASM_DEPLOY_INGRESS_AUTH_READ_TOKEN",
                "read-token-1234567890",
            ),
            (
                "WASM_DEPLOY_INGRESS_AUTH_WRITE_TOKEN",
                "write-token-1234567890",
            ),
        ],
    )
    .await
    .expect("Failed to start deploy ingress");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("Failed to build HTTP client");
    let deploy_url = format!("http://127.0.0.1:{deploy_port}");

    let health = client
        .get(format!("{deploy_url}/health"))
        .send()
        .await
        .expect("Failed to query health endpoint");
    assert_eq!(health.status(), StatusCode::OK);

    let credential_body = serde_json::json!({
        "key": "registry-reader",
        "value": "authorization:Bearer registry-token",
    });

    let unauthorized_write = client
        .put(format!("{deploy_url}/deploy/artifact-credentials"))
        .json(&credential_body)
        .send()
        .await
        .expect("Failed to send unauthorized write request");
    assert_eq!(unauthorized_write.status(), StatusCode::UNAUTHORIZED);

    let forbidden_write = client
        .put(format!("{deploy_url}/deploy/artifact-credentials"))
        .bearer_auth("read-token-1234567890")
        .json(&credential_body)
        .send()
        .await
        .expect("Failed to send read-token write request");
    assert_eq!(forbidden_write.status(), StatusCode::FORBIDDEN);

    let authorized_write = client
        .put(format!("{deploy_url}/deploy/artifact-credentials"))
        .bearer_auth("write-token-1234567890")
        .json(&credential_body)
        .send()
        .await
        .expect("Failed to send write-token write request");
    assert_eq!(authorized_write.status(), StatusCode::OK);

    let artifact_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let unauthorized_read = client
        .get(format!(
            "{deploy_url}/artifacts/{artifact_sha}/verification"
        ))
        .send()
        .await
        .expect("Failed to send unauthorized read request");
    assert_eq!(unauthorized_read.status(), StatusCode::UNAUTHORIZED);

    let read_token_read = client
        .get(format!(
            "{deploy_url}/artifacts/{artifact_sha}/verification"
        ))
        .bearer_auth("read-token-1234567890")
        .send()
        .await
        .expect("Failed to send read-token read request");
    assert_eq!(read_token_read.status(), StatusCode::NOT_FOUND);

    let write_token_read = client
        .get(format!(
            "{deploy_url}/artifacts/{artifact_sha}/verification"
        ))
        .bearer_auth("write-token-1234567890")
        .send()
        .await
        .expect("Failed to send write-token read request");
    assert_eq!(write_token_read.status(), StatusCode::NOT_FOUND);

    let deploy_with_read_token = client
        .post(format!("{deploy_url}/deploy/intent"))
        .bearer_auth("read-token-1234567890")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Failed to send read-token deploy request");
    assert_eq!(deploy_with_read_token.status(), StatusCode::FORBIDDEN);

    let deploy_with_write_token = client
        .post(format!("{deploy_url}/deploy/intent"))
        .bearer_auth("write-token-1234567890")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Failed to send write-token deploy request");
    assert_eq!(
        deploy_with_write_token.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    drop(deploy_ingress);
    drop(nats);
}

#[tokio::test]
async fn test_deploy_ingress_rejects_malformed_auth_and_oversized_body() {
    let nats_port = reserve_test_port().expect("Failed to reserve NATS port");
    let deploy_port = reserve_test_port().expect("Failed to reserve deploy ingress port");
    let artifact_port = reserve_test_port().expect("Failed to reserve artifact ingress port");

    let nats = NatsContainer::start(nats_port)
        .await
        .expect("Failed to start NATS");
    let deploy_ingress = DeployIngressProcess::start_with_env(
        "deploy-ingress-edge-auth",
        &nats.url,
        deploy_port,
        artifact_port,
        &[
            ("WASM_DEPLOY_INGRESS_HA_ENABLED", "false"),
            ("WASM_DEPLOY_INGRESS_AUTH_ENABLED", "true"),
            (
                "WASM_DEPLOY_INGRESS_AUTH_READ_TOKEN",
                "read-token-1234567890",
            ),
            (
                "WASM_DEPLOY_INGRESS_AUTH_WRITE_TOKEN",
                "write-token-1234567890",
            ),
        ],
    )
    .await
    .expect("Failed to start deploy ingress");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("Failed to build HTTP client");
    let deploy_url = format!("http://127.0.0.1:{deploy_port}");
    let artifact_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    let malformed_auth = client
        .get(format!(
            "{deploy_url}/artifacts/{artifact_sha}/verification"
        ))
        .header(reqwest::header::AUTHORIZATION, "Token not-a-bearer-token")
        .send()
        .await
        .expect("Failed to send malformed auth request");
    assert_eq!(malformed_auth.status(), StatusCode::UNAUTHORIZED);

    let oversized_body = "x".repeat(300 * 1024);
    let oversized = client
        .post(format!("{deploy_url}/deploy/intent"))
        .bearer_auth("write-token-1234567890")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(oversized_body)
        .send()
        .await
        .expect("Failed to send oversized request");
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);

    drop(deploy_ingress);
    drop(nats);
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
    let mut health_response = None;
    for _ in 0..10 {
        let response = send_request(node.proxy_port, "test-app.local", "/health")
            .await
            .expect("Failed to send health request");
        if response.status() == 200 {
            health_response = Some(response);
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
    let health_response = health_response.expect("App did not return 200 for /health in time");
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
async fn test_deploy_ingress_follower_rejects_mutation_with_leader_hint() {
    let nats_port = reserve_test_port().expect("Failed to reserve NATS port");
    let leader_deploy_port = reserve_test_port().expect("Failed to reserve leader deploy port");
    let leader_artifact_port = reserve_test_port().expect("Failed to reserve leader artifact port");
    let follower_deploy_port = reserve_test_port().expect("Failed to reserve follower deploy port");
    let follower_artifact_port =
        reserve_test_port().expect("Failed to reserve follower artifact port");

    let nats = NatsContainer::start(nats_port)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let leader = DeployIngressProcess::start(
        "test-deploy-ingress-leader",
        &nats.url,
        leader_deploy_port,
        leader_artifact_port,
    )
    .await
    .expect("Failed to start leader deploy ingress");
    let follower = DeployIngressProcess::start(
        "test-deploy-ingress-follower",
        &nats.url,
        follower_deploy_port,
        follower_artifact_port,
    )
    .await
    .expect("Failed to start follower deploy ingress");

    let leader_health = wait_for_deploy_ingress_role(leader.deploy_port, true)
        .await
        .expect("Leader deploy ingress did not become leader");
    let follower_health = wait_for_deploy_ingress_role(follower.deploy_port, false)
        .await
        .expect("Follower deploy ingress did not become follower");

    assert!(leader_health.ha_enabled);
    assert_eq!(leader_health.ingress_id, "test-deploy-ingress-leader");
    let expected_leader_artifact_url = format!("http://127.0.0.1:{leader_artifact_port}");
    assert_eq!(
        follower_health.leader_ingress_id.as_deref(),
        Some("test-deploy-ingress-leader")
    );
    assert_eq!(
        follower_health.leader_artifact_server_url.as_deref(),
        Some(expected_leader_artifact_url.as_str())
    );

    let response = reqwest::Client::new()
        .put(format!(
            "http://127.0.0.1:{}/deploy/artifact-credentials",
            follower.deploy_port
        ))
        .json(&serde_json::json!({
            "key": "ghcr-reader",
            "value": "authorization:Bearer token"
        }))
        .send()
        .await
        .expect("Failed to call follower deploy ingress");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response
        .json::<serde_json::Value>()
        .await
        .expect("Failed to decode follower rejection body");
    assert_eq!(
        body.get("error").and_then(|v| v.as_str()),
        Some("deploy_ingress_not_leader")
    );
    assert_eq!(
        body.get("leader_ingress_id").and_then(|v| v.as_str()),
        Some("test-deploy-ingress-leader")
    );
    assert_eq!(
        body.get("leader_artifact_server_url")
            .and_then(|v| v.as_str()),
        Some(expected_leader_artifact_url.as_str())
    );

    leader.stop().ok();
    follower.stop().ok();
}

#[tokio::test]
async fn test_deploy_ingress_follower_promotes_and_accepts_write_after_leader_exit() {
    let nats_port = reserve_test_port().expect("Failed to reserve NATS port");
    let leader_deploy_port = reserve_test_port().expect("Failed to reserve leader deploy port");
    let leader_artifact_port = reserve_test_port().expect("Failed to reserve leader artifact port");
    let follower_deploy_port = reserve_test_port().expect("Failed to reserve follower deploy port");
    let follower_artifact_port =
        reserve_test_port().expect("Failed to reserve follower artifact port");

    let nats = NatsContainer::start(nats_port)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let ha_env = [
        ("WASM_DEPLOY_INGRESS_HA_LEASE_TTL_SECS", "4"),
        ("WASM_DEPLOY_INGRESS_HA_LEASE_REFRESH_SECS", "1"),
        ("WASM_DEPLOY_INGRESS_KEY_SOURCE", "env:WCP_TEST_SHARED_KEK"),
        (
            "WCP_TEST_SHARED_KEK",
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        ),
    ];

    let leader = DeployIngressProcess::start_with_env(
        "test-deploy-ingress-failover-leader",
        &nats.url,
        leader_deploy_port,
        leader_artifact_port,
        &ha_env,
    )
    .await
    .expect("Failed to start leader deploy ingress");
    let follower = DeployIngressProcess::start_with_env(
        "test-deploy-ingress-failover-follower",
        &nats.url,
        follower_deploy_port,
        follower_artifact_port,
        &ha_env,
    )
    .await
    .expect("Failed to start follower deploy ingress");

    wait_for_deploy_ingress_role(leader.deploy_port, true)
        .await
        .expect("Leader deploy ingress did not become leader");
    wait_for_deploy_ingress_role(follower.deploy_port, false)
        .await
        .expect("Follower deploy ingress did not become follower");

    leader.stop().ok();

    let promoted = wait_for_deploy_ingress_role(follower.deploy_port, true)
        .await
        .expect("Follower did not promote to leader after leader exit");
    assert_eq!(promoted.ingress_id, "test-deploy-ingress-failover-follower");
    assert_eq!(
        promoted.leader_ingress_id.as_deref(),
        Some("test-deploy-ingress-failover-follower")
    );
    let expected_promoted_artifact_url = format!("http://127.0.0.1:{follower_artifact_port}");
    assert_eq!(
        promoted.leader_artifact_server_url.as_deref(),
        Some(expected_promoted_artifact_url.as_str())
    );

    let response = reqwest::Client::new()
        .put(format!(
            "http://127.0.0.1:{}/deploy/artifact-credentials",
            follower.deploy_port
        ))
        .json(&serde_json::json!({
            "key": "ghcr-reader",
            "value": "authorization:Bearer promoted-token"
        }))
        .send()
        .await
        .expect("Failed to call promoted deploy ingress");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .json::<serde_json::Value>()
        .await
        .expect("Failed to decode promoted ingress response");
    assert_eq!(
        body.get("key").and_then(|v| v.as_str()),
        Some("ghcr-reader")
    );

    let audit_records =
        read_audit_records(&follower.audit_path).expect("Failed to read follower audit records");
    let credential_write = audit_records
        .iter()
        .find(|record| {
            record.get("event").and_then(|v| v.as_str()) == Some("artifact_credential_set")
        })
        .expect("Missing artifact_credential_set audit record after promotion");
    assert_eq!(
        credential_write.get("key").and_then(|v| v.as_str()),
        Some("ghcr-reader")
    );

    follower.stop().ok();
}

#[tokio::test]
async fn test_deploy_ingress_follower_can_deploy_with_replicated_credential_after_failover() {
    let deployed_app_id = "default/hello-axum-failover-oci:v1";

    let nats_port = reserve_test_port().expect("Failed to reserve NATS port");
    let proxy_port = reserve_test_port().expect("Failed to reserve proxy port");
    let artifact_port = reserve_test_port().expect("Failed to reserve artifact port");
    let admin_port = reserve_test_port().expect("Failed to reserve admin port");
    let leader_deploy_port = reserve_test_port().expect("Failed to reserve leader deploy port");
    let leader_artifact_port = reserve_test_port().expect("Failed to reserve leader artifact port");
    let follower_deploy_port = reserve_test_port().expect("Failed to reserve follower deploy port");
    let follower_artifact_port =
        reserve_test_port().expect("Failed to reserve follower artifact port");

    let nats = NatsContainer::start(nats_port)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start_with_admin(
        "test-node-failover-oci-0",
        &nats.url,
        proxy_port,
        artifact_port,
        admin_port,
    )
    .await
    .expect("Failed to start node");

    let ha_env = [
        ("WASM_DEPLOY_INGRESS_HA_LEASE_TTL_SECS", "4"),
        ("WASM_DEPLOY_INGRESS_HA_LEASE_REFRESH_SECS", "1"),
        ("WASM_DEPLOY_INGRESS_KEY_SOURCE", "env:WCP_TEST_SHARED_KEK"),
        (
            "WCP_TEST_SHARED_KEK",
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        ),
    ];

    let leader = DeployIngressProcess::start_with_env(
        "test-deploy-ingress-credential-leader",
        &nats.url,
        leader_deploy_port,
        leader_artifact_port,
        &ha_env,
    )
    .await
    .expect("Failed to start leader deploy ingress");
    let follower = DeployIngressProcess::start_with_env(
        "test-deploy-ingress-credential-follower",
        &nats.url,
        follower_deploy_port,
        follower_artifact_port,
        &ha_env,
    )
    .await
    .expect("Failed to start follower deploy ingress");

    wait_for_deploy_ingress_role(leader.deploy_port, true)
        .await
        .expect("Leader deploy ingress did not become leader");
    wait_for_deploy_ingress_role(follower.deploy_port, false)
        .await
        .expect("Follower deploy ingress did not become follower");

    let leader_deploy_api = format!("http://127.0.0.1:{}", leader.deploy_port);
    let follower_deploy_api = format!("http://127.0.0.1:{}", follower.deploy_port);
    let node_api = format!("http://127.0.0.1:{}", node.admin_port);

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
        leader_deploy_api.clone(),
    )
    .await
    .expect("Failed to store artifact credential on leader ingress");

    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read hello-axum.wasm");
    let (_registry_handle, registry_port, wasm_hash) = start_mock_oci_registry(wasm_bytes)
        .await
        .expect("Failed to start local OCI registry");

    leader.stop().ok();

    let promoted = wait_for_deploy_ingress_role(follower.deploy_port, true)
        .await
        .expect("Follower did not promote to leader after leader exit");
    assert_eq!(
        promoted.leader_ingress_id.as_deref(),
        Some("test-deploy-ingress-credential-follower")
    );

    run_ctl_async(
        vec![
            "deploy".to_string(),
            "--app".to_string(),
            "hello-axum-failover-oci".to_string(),
            "--version".to_string(),
            "v1".to_string(),
            "--artifact-ref".to_string(),
            format!("oci://127.0.0.1:{registry_port}/example-org/hello-axum:v1"),
            "--artifact-credential".to_string(),
            "ghcr-reader".to_string(),
        ],
        nats.url.clone(),
        node_api.clone(),
        follower_deploy_api.clone(),
    )
    .await
    .expect("Failed to deploy app through promoted follower ingress");

    add_route(&bus, "test-failover-oci.local", deployed_app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "test-failover-oci.local", 30)
        .await
        .expect("Failover OCI-deployed app did not become ready");

    let response = send_request(node.proxy_port, "test-failover-oci.local", "/")
        .await
        .expect("Failed to send request to failover-deployed app");
    assert_eq!(response.status(), 200);

    let verification = reqwest::Client::new()
        .get(format!(
            "{}/artifacts/{}/verification",
            follower_deploy_api, wasm_hash
        ))
        .send()
        .await
        .expect("Failed to query promoted follower verification record");
    assert_eq!(verification.status(), 200);

    follower.stop().ok();
    node.stop().ok();
}

#[tokio::test]
async fn test_promoted_deploy_ingress_fanout_covers_multiple_active_nodes() {
    let deployed_app_id = "default/hello-axum-multi-node-oci:v1";

    let nats_port = reserve_test_port().expect("Failed to reserve NATS port");

    let node_a_proxy_port = reserve_test_port().expect("Failed to reserve node A proxy port");
    let node_a_artifact_port = reserve_test_port().expect("Failed to reserve node A artifact port");
    let node_a_admin_port = reserve_test_port().expect("Failed to reserve node A admin port");

    let node_b_proxy_port = reserve_test_port().expect("Failed to reserve node B proxy port");
    let node_b_artifact_port = reserve_test_port().expect("Failed to reserve node B artifact port");
    let node_b_admin_port = reserve_test_port().expect("Failed to reserve node B admin port");

    let leader_deploy_port = reserve_test_port().expect("Failed to reserve leader deploy port");
    let leader_artifact_port = reserve_test_port().expect("Failed to reserve leader artifact port");
    let follower_deploy_port = reserve_test_port().expect("Failed to reserve follower deploy port");
    let follower_artifact_port =
        reserve_test_port().expect("Failed to reserve follower artifact port");

    let nats = NatsContainer::start(nats_port)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node_a = NodeProcess::start_with_admin(
        "test-node-multi-oci-0",
        &nats.url,
        node_a_proxy_port,
        node_a_artifact_port,
        node_a_admin_port,
    )
    .await
    .expect("Failed to start node A");
    let node_b = NodeProcess::start_with_admin(
        "test-node-multi-oci-1",
        &nats.url,
        node_b_proxy_port,
        node_b_artifact_port,
        node_b_admin_port,
    )
    .await
    .expect("Failed to start node B");

    let ha_env = [
        ("WASM_DEPLOY_INGRESS_HA_LEASE_TTL_SECS", "4"),
        ("WASM_DEPLOY_INGRESS_HA_LEASE_REFRESH_SECS", "1"),
        ("WASM_DEPLOY_INGRESS_KEY_SOURCE", "env:WCP_TEST_SHARED_KEK"),
        (
            "WCP_TEST_SHARED_KEK",
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        ),
    ];

    let leader = DeployIngressProcess::start_with_env(
        "test-deploy-ingress-multi-node-leader",
        &nats.url,
        leader_deploy_port,
        leader_artifact_port,
        &ha_env,
    )
    .await
    .expect("Failed to start leader deploy ingress");
    let follower = DeployIngressProcess::start_with_env(
        "test-deploy-ingress-multi-node-follower",
        &nats.url,
        follower_deploy_port,
        follower_artifact_port,
        &ha_env,
    )
    .await
    .expect("Failed to start follower deploy ingress");

    wait_for_deploy_ingress_role(leader.deploy_port, true)
        .await
        .expect("Leader deploy ingress did not become leader");
    wait_for_deploy_ingress_role(follower.deploy_port, false)
        .await
        .expect("Follower deploy ingress did not become follower");

    let leader_deploy_api = format!("http://127.0.0.1:{}", leader.deploy_port);
    let follower_deploy_api = format!("http://127.0.0.1:{}", follower.deploy_port);
    let node_api = format!("http://127.0.0.1:{}", node_a.admin_port);

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
        leader_deploy_api.clone(),
    )
    .await
    .expect("Failed to store artifact credential on leader ingress");

    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read hello-axum.wasm");
    let (_registry_handle, registry_port, wasm_hash) = start_mock_oci_registry(wasm_bytes)
        .await
        .expect("Failed to start local OCI registry");

    leader.stop().ok();

    wait_for_deploy_ingress_role(follower.deploy_port, true)
        .await
        .expect("Follower did not promote to leader after leader exit");

    let deploy_request = common::deploy::DeployIntentRequest {
        app_id: common::types::AppId(deployed_app_id.to_string()),
        config: build_app_config(deployed_app_id, 100_000_000, 100, 4),
        gateway_config: None,
        api_keys: vec![],
        artifact: common::deploy::RemoteArtifactSource {
            reference: Some(format!(
                "oci://127.0.0.1:{registry_port}/example-org/hello-axum:v1"
            )),
            url: String::new(),
            sha256: String::new(),
            credential_ref: Some("ghcr-reader".to_string()),
            signature: None,
        },
    };

    let deploy_response = reqwest::Client::new()
        .post(format!("{}/deploy/intent", follower_deploy_api))
        .json(&deploy_request)
        .send()
        .await
        .expect("Failed to submit deploy intent to promoted follower");
    assert_eq!(deploy_response.status(), StatusCode::ACCEPTED);
    let deploy_response = deploy_response
        .json::<common::deploy::DeployIntentResponse>()
        .await
        .expect("Failed to decode deploy intent response");

    assert_eq!(deploy_response.app_id.0, deployed_app_id);
    assert_eq!(deploy_response.expected_hash, wasm_hash);
    assert_eq!(deploy_response.artifact_transfer_manifests.len(), 2);

    let audience_ids = deploy_response
        .artifact_transfer_manifests
        .iter()
        .map(|binding| binding.audience_node_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        audience_ids,
        std::collections::BTreeSet::from([node_a.node_id.as_str(), node_b.node_id.as_str(),])
    );
    for binding in &deploy_response.artifact_transfer_manifests {
        assert_eq!(
            binding.artifact_transfer_manifest.manifest.artifact_sha256,
            wasm_hash
        );
        assert_eq!(
            binding
                .artifact_transfer_manifest
                .manifest
                .audience
                .as_deref(),
            Some(binding.audience_node_id.as_str())
        );
    }

    add_route(&bus, "test-multi-node-oci.local", deployed_app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node_a.proxy_port, "test-multi-node-oci.local", 30)
        .await
        .expect("Node A did not become ready after multi-node deploy");
    wait_for_app_ready(node_b.proxy_port, "test-multi-node-oci.local", 30)
        .await
        .expect("Node B did not become ready after multi-node deploy");

    follower.stop().ok();
    node_a.stop().ok();
    node_b.stop().ok();
}

#[tokio::test]
async fn test_deploy_and_serve_http_via_oci_artifact_ref() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};
    use tokio::time::sleep;

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
    let (_registry_handle, registry_port, wasm_hash) = start_mock_oci_registry(wasm_bytes.clone())
        .await
        .expect("Failed to start local OCI registry");

    let node_api = format!("http://127.0.0.1:{}", node.admin_port);
    let deploy_api = format!("http://127.0.0.1:{}", deploy_ingress.deploy_port);

    let signing_key = SigningKey::from_bytes(&[11u8; 32]);
    let signed_claims = serde_json::to_vec(&serde_json::json!({
        "sha256": wasm_hash,
        "issuer": "https://token.actions.githubusercontent.com",
        "identity": serde_json::Value::Null,
        "repository": "example-org/hello-axum",
        "namespace": "default",
    }))
    .expect("Failed to serialize signed artifact claims");
    let artifact_signature = signing_key.sign(&signed_claims);

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
            format!("oci://127.0.0.1:{registry_port}/example-org/hello-axum:v1"),
            "--artifact-credential".to_string(),
            "ghcr-reader".to_string(),
            "--artifact-public-key".to_string(),
            STANDARD.encode(signing_key.verifying_key().to_bytes()),
            "--artifact-signature".to_string(),
            STANDARD.encode(artifact_signature.to_bytes()),
            "--artifact-issuer".to_string(),
            "https://token.actions.githubusercontent.com".to_string(),
            "--artifact-repository".to_string(),
            "example-org/hello-axum".to_string(),
            "--artifact-namespace".to_string(),
            "default".to_string(),
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

    let verification = reqwest::Client::new()
        .get(format!(
            "{}/artifacts/{}/verification",
            deploy_api, wasm_hash
        ))
        .send()
        .await
        .expect("Failed to query deploy-ingress verification record");
    assert_eq!(verification.status(), 200);
    let verification = verification
        .json::<common::deploy::ArtifactVerificationRecord>()
        .await
        .expect("Failed to decode verification record");
    assert!(verification.verified);
    assert_eq!(
        verification.issuer.as_deref(),
        Some("https://token.actions.githubusercontent.com")
    );
    assert_eq!(
        verification.repository.as_deref(),
        Some("example-org/hello-axum")
    );
    assert_eq!(verification.namespace.as_deref(), Some("default"));

    let audit_records = read_audit_records(&deploy_ingress.audit_path)
        .expect("Failed to read deploy-ingress audit records");
    let accepted = audit_records
        .iter()
        .find(|record| {
            record.get("event").and_then(|v| v.as_str()) == Some("deploy_intent_accepted")
        })
        .expect("Missing deploy_intent_accepted audit record");
    assert_eq!(
        accepted.get("app_id").and_then(|v| v.as_str()),
        Some(deployed_app_id)
    );
    assert_eq!(
        accepted
            .get("artifact_verification")
            .and_then(|v| v.get("verified"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        accepted
            .get("artifact_verification")
            .and_then(|v| v.get("repository"))
            .and_then(|v| v.as_str()),
        Some("example-org/hello-axum")
    );

    node.stop().ok();
    deploy_ingress.stop().ok();
}

#[tokio::test]
async fn test_deploy_via_oci_artifact_ref_rejected_by_signature_policy() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};
    use tokio::time::sleep;

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
        "test-node-oci-reject-0",
        &nats.url,
        proxy_port,
        artifact_port,
        admin_port,
    )
    .await
    .expect("Failed to start node");

    let deploy_port = reserve_test_port().expect("Failed to reserve deploy ingress port");
    let deploy_artifact_port = reserve_test_port().expect("Failed to reserve deploy artifact port");
    let deploy_ingress = DeployIngressProcess::start_with_env(
        "test-deploy-ingress-reject-0",
        &nats.url,
        deploy_port,
        deploy_artifact_port,
        &[
            ("WASM_DEPLOY_INGRESS_REQUIRE_SIGNATURE", "true"),
            (
                "WASM_DEPLOY_INGRESS_ALLOWED_REPOSITORIES",
                "example-org/other-app",
            ),
        ],
    )
    .await
    .expect("Failed to start deploy ingress");
    wait_for_deploy_intent_ready(deploy_ingress.deploy_port)
        .await
        .expect("Deploy intent route did not become ready");

    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read hello-axum.wasm");
    let (_registry_handle, registry_port, wasm_hash) = start_mock_oci_registry(wasm_bytes)
        .await
        .expect("Failed to start local OCI registry");

    let node_api = format!("http://127.0.0.1:{}", node.admin_port);
    let deploy_api = format!("http://127.0.0.1:{}", deploy_ingress.deploy_port);

    let signing_key = SigningKey::from_bytes(&[17u8; 32]);
    let signed_claims = serde_json::to_vec(&serde_json::json!({
        "sha256": wasm_hash,
        "issuer": "https://token.actions.githubusercontent.com",
        "identity": serde_json::Value::Null,
        "repository": "example-org/hello-axum",
        "namespace": "default",
    }))
    .expect("Failed to serialize signed artifact claims");
    let artifact_signature = signing_key.sign(&signed_claims);

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

    let deploy_result = run_ctl_async(
        vec![
            "deploy".to_string(),
            "--app".to_string(),
            "hello-axum-oci-reject".to_string(),
            "--version".to_string(),
            "v1".to_string(),
            "--artifact-ref".to_string(),
            format!("oci://127.0.0.1:{registry_port}/example-org/hello-axum:v1"),
            "--artifact-credential".to_string(),
            "ghcr-reader".to_string(),
            "--artifact-public-key".to_string(),
            STANDARD.encode(signing_key.verifying_key().to_bytes()),
            "--artifact-signature".to_string(),
            STANDARD.encode(artifact_signature.to_bytes()),
            "--artifact-issuer".to_string(),
            "https://token.actions.githubusercontent.com".to_string(),
            "--artifact-repository".to_string(),
            "example-org/hello-axum".to_string(),
            "--artifact-namespace".to_string(),
            "default".to_string(),
        ],
        nats.url.clone(),
        node_api.clone(),
        deploy_api.clone(),
    )
    .await;

    let error = deploy_result.expect_err("Deploy should be rejected by repository allowlist");
    assert!(
        error.contains("artifact signature repository claim 'example-org/hello-axum' is not allowed by deploy-ingress policy"),
        "unexpected deploy error: {error}"
    );

    let verification = reqwest::Client::new()
        .get(format!(
            "{}/artifacts/{}/verification",
            deploy_api, wasm_hash
        ))
        .send()
        .await
        .expect("Failed to query deploy-ingress verification record");
    assert_eq!(verification.status(), 404);

    let expected_reference = format!("oci://127.0.0.1:{registry_port}/example-org/hello-axum:v1");
    let audit_records = read_audit_records(&deploy_ingress.audit_path)
        .expect("Failed to read deploy-ingress audit records");
    let rejected = audit_records
        .iter()
        .find(|record| {
            record.get("event").and_then(|v| v.as_str()) == Some("deploy_intent_rejected")
        })
        .expect("Missing deploy_intent_rejected audit record");
    assert_eq!(
        rejected.get("app_id").and_then(|v| v.as_str()),
        Some("default/hello-axum-oci-reject:v1")
    );
    assert_eq!(
        rejected
            .get("artifact_source_reference")
            .and_then(|v| v.as_str()),
        Some(expected_reference.as_str())
    );
    assert!(rejected
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("repository claim 'example-org/hello-axum' is not allowed"));

    node.stop().ok();
    deploy_ingress.stop().ok();
}

#[tokio::test]
async fn test_deploy_via_oci_tag_ref_rejected_by_digest_pin_policy() {
    use tokio::time::sleep;

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
        "test-node-oci-digest-policy-0",
        &nats.url,
        proxy_port,
        artifact_port,
        admin_port,
    )
    .await
    .expect("Failed to start node");

    let deploy_port = reserve_test_port().expect("Failed to reserve deploy ingress port");
    let deploy_artifact_port = reserve_test_port().expect("Failed to reserve deploy artifact port");
    let deploy_ingress = DeployIngressProcess::start_with_env(
        "test-deploy-ingress-digest-policy-0",
        &nats.url,
        deploy_port,
        deploy_artifact_port,
        &[("WASM_DEPLOY_INGRESS_REQUIRE_OCI_DIGEST_REFS", "true")],
    )
    .await
    .expect("Failed to start deploy ingress");
    wait_for_deploy_intent_ready(deploy_ingress.deploy_port)
        .await
        .expect("Deploy intent route did not become ready");

    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read hello-axum.wasm");
    let (_registry_handle, registry_port, wasm_hash) = start_mock_oci_registry(wasm_bytes)
        .await
        .expect("Failed to start local OCI registry");

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

    let deploy_result = run_ctl_async(
        vec![
            "deploy".to_string(),
            "--app".to_string(),
            "hello-axum-oci-digest-policy".to_string(),
            "--version".to_string(),
            "v1".to_string(),
            "--artifact-ref".to_string(),
            format!("oci://127.0.0.1:{registry_port}/example-org/hello-axum:v1"),
            "--artifact-credential".to_string(),
            "ghcr-reader".to_string(),
        ],
        nats.url.clone(),
        node_api.clone(),
        deploy_api.clone(),
    )
    .await;

    let error = deploy_result.expect_err("Deploy should be rejected by OCI digest pin policy");
    assert!(
        error
            .contains("deploy-ingress policy requires OCI artifact references to be digest-pinned"),
        "unexpected deploy error: {error}"
    );

    let verification = reqwest::Client::new()
        .get(format!(
            "{}/artifacts/{}/verification",
            deploy_api, wasm_hash
        ))
        .send()
        .await
        .expect("Failed to query deploy-ingress verification record");
    assert_eq!(verification.status(), 404);

    let expected_reference = format!("oci://127.0.0.1:{registry_port}/example-org/hello-axum:v1");
    let audit_records = read_audit_records(&deploy_ingress.audit_path)
        .expect("Failed to read deploy-ingress audit records");
    let rejected = audit_records
        .iter()
        .find(|record| {
            record.get("event").and_then(|v| v.as_str()) == Some("deploy_intent_rejected")
        })
        .expect("Missing deploy_intent_rejected audit record");
    assert_eq!(
        rejected.get("app_id").and_then(|v| v.as_str()),
        Some("default/hello-axum-oci-digest-policy:v1")
    );
    assert_eq!(
        rejected
            .get("artifact_source_reference")
            .and_then(|v| v.as_str()),
        Some(expected_reference.as_str())
    );
    assert!(rejected
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("digest-pinned"));

    node.stop().ok();
    deploy_ingress.stop().ok();
}
