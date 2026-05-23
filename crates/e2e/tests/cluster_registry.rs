use common::artifact_transfer::{ArtifactManifestBatchRequest, ArtifactManifestBatchResponse};
use common::types::ClusterNodeRecord;
use e2e::fixture::ClusterFixture;
use e2e::helpers;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

static NODE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(serde::Deserialize)]
struct ClusterRegistryResponse {
    nodes: Vec<ClusterNodeRecord>,
    active_staleness_secs: u64,
}

fn admin_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

fn encode_app_id(app_id: &str) -> String {
    app_id.replace(':', "%3A")
}

async fn instance_count(
    http: &reqwest::Client,
    admin_port: u16,
    app_id: &str,
) -> Result<u64, String> {
    let url = admin_url(
        admin_port,
        &format!("/admin/instances/{}", encode_app_id(app_id)),
    );
    let body: serde_json::Value = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("instance count request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("failed to decode instance count response: {e}"))?;
    Ok(body
        .get("count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0))
}

async fn wait_for_registry(
    http: &reqwest::Client,
    admin_port: u16,
    expected_node_ids: &[&str],
    timeout: Duration,
) -> Result<Vec<ClusterNodeRecord>, String> {
    let deadline = Instant::now() + timeout;
    let url = admin_url(admin_port, "/admin/cluster/nodes");

    loop {
        match http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: ClusterRegistryResponse = resp
                    .json()
                    .await
                    .map_err(|e| format!("failed to decode cluster registry response: {e}"))?;
                let nodes = body.nodes;
                if body.active_staleness_secs == 0 {
                    return Err("cluster registry returned invalid active_staleness_secs=0".into());
                }

                let ids: Vec<&str> = nodes.iter().map(|node| node.node_id.as_str()).collect();
                if expected_node_ids
                    .iter()
                    .all(|expected| ids.contains(expected))
                {
                    return Ok(nodes);
                }
            }
            Ok(_) | Err(_) => {}
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "cluster registry on admin port {admin_port} did not converge to {:?} within {:?}",
                expected_node_ids, timeout
            ));
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_instance_port(
    node: &e2e::fixture::NodeProcess,
    timeout: Duration,
) -> Result<(u16, reqwest::Response), String> {
    let deadline = Instant::now() + timeout;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| format!("failed to build instance-port probe client: {e}"))?;
    loop {
        for port in node.port_start()..=node.port_end() {
            let url = format!("http://127.0.0.1:{port}/");
            if let Ok(response) = client.get(&url).send().await {
                if response.status().is_success() {
                    return Ok((port, response));
                }
            }
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "no listening instance port found in {}-{} within {:?}",
                node.port_start(),
                node.port_end(),
                timeout
            ));
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn local_non_loopback_ip() -> Result<IpAddr, String> {
    let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        .map_err(|e| format!("failed to bind UDP probe socket: {e}"))?;
    socket
        .connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 80))
        .map_err(|e| format!("failed to connect UDP probe socket: {e}"))?;
    let ip = socket
        .local_addr()
        .map_err(|e| format!("failed to read UDP probe local addr: {e}"))?
        .ip();
    if ip.is_loopback() {
        return Err(
            "resolved local IP is loopback; cannot verify hardened non-loopback rejection".into(),
        );
    }
    Ok(ip)
}

#[tokio::test]
#[ignore = "live cluster regression; run explicitly or via CI E2E lane"]
async fn test_live_cluster_registry_drives_artifact_authorize_audience_set() {
    let _guard = NODE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let cluster = ClusterFixture::dual()
        .await
        .expect("failed to start two-node cluster fixture");
    let http = reqwest::Client::new();

    let node0 = cluster.node(0);
    let node1 = cluster.node(1);
    let expected_ids = [node0.node_id.as_str(), node1.node_id.as_str()];

    let registry0 = wait_for_registry(
        &http,
        node0.admin_port,
        &expected_ids,
        Duration::from_secs(45),
    )
    .await
    .expect("node 0 registry did not converge");
    let registry1 = wait_for_registry(
        &http,
        node1.admin_port,
        &expected_ids,
        Duration::from_secs(45),
    )
    .await
    .expect("node 1 registry did not converge");

    assert_eq!(
        registry0.len(),
        2,
        "node 0 registry should contain both live nodes"
    );
    assert_eq!(
        registry1.len(),
        2,
        "node 1 registry should contain both live nodes"
    );
    assert!(
        registry0.iter().any(|node| node.node_id == node1.node_id
            && node.proxy_address.as_deref() == Some(node1.proxy_addr_str().as_str())),
        "node 0 registry should carry node 1's routable proxy address"
    );
    assert!(
        registry1.iter().any(|node| node.node_id == node0.node_id
            && node.proxy_address.as_deref() == Some(node0.proxy_addr_str().as_str())),
        "node 1 registry should carry node 0's routable proxy address"
    );

    let wasm_path = helpers::find_echo_service_wasm().expect("echo-service.wasm not found");
    let sha256 = helpers::sha256_file(&wasm_path).expect("failed to hash wasm artifact");
    helpers::upload_artifact(node0.artifact_port, &wasm_path, &sha256)
        .await
        .expect("failed to upload artifact to node 0");

    let peer_ids: Vec<String> = registry0
        .iter()
        .filter(|node| node.node_id != node0.node_id)
        .map(|node| node.node_id.clone())
        .collect();
    assert_eq!(peer_ids, vec![node1.node_id.clone()]);

    let response = http
        .post(admin_url(
            node0.artifact_port,
            &format!("/artifacts/{sha256}/authorize"),
        ))
        .json(&ArtifactManifestBatchRequest {
            audiences: peer_ids.clone(),
        })
        .send()
        .await
        .expect("authorize request failed");
    assert!(
        response.status().is_success(),
        "authorize endpoint returned {}",
        response.status()
    );

    let batch: ArtifactManifestBatchResponse = response
        .json()
        .await
        .expect("failed to decode authorize response");
    assert_eq!(batch.sha256, sha256);
    assert_eq!(batch.manifests.len(), 1);
    assert_eq!(batch.manifests[0].audience_node_id, node1.node_id);
    assert_eq!(
        batch.manifests[0]
            .artifact_transfer_manifest
            .manifest
            .audience
            .as_deref(),
        Some(node1.node_id.as_str())
    );
}

#[tokio::test]
#[ignore = "live cluster regression; run explicitly or via CI E2E lane"]
async fn test_live_overloaded_node_routes_first_request_to_remote_proxy() {
    let _guard = NODE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let cluster = ClusterFixture::dual()
        .await
        .expect("failed to start two-node cluster fixture");
    let http = reqwest::Client::new();
    let bus = cluster
        .connect_bus()
        .await
        .expect("failed to connect test bus");

    let node0 = cluster.node(0);
    let node1 = cluster.node(1);
    let expected_ids = [node0.node_id.as_str(), node1.node_id.as_str()];

    let registry0 = wait_for_registry(
        &http,
        node0.admin_port,
        &expected_ids,
        Duration::from_secs(45),
    )
    .await
    .expect("node 0 registry did not converge");
    assert!(
        registry0.iter().any(|node| {
            node.node_id == node1.node_id
                && node.proxy_address.as_deref() == Some(node1.proxy_addr_str().as_str())
        }),
        "node 0 should learn node 1's routable proxy address"
    );

    let app_id = "hello-remote:v1";
    let host = "hello-remote.local";
    let wasm_path = helpers::find_hello_axum_wasm().expect("hello-axum.wasm not found");
    let sha256 = helpers::sha256_file(&wasm_path).expect("failed to hash hello app");
    let size_bytes = std::fs::metadata(&wasm_path)
        .expect("failed to stat hello app")
        .len();
    let config = helpers::build_app_config(app_id, 100_000_000, 100, 1);

    // Seed the artifact on both nodes so this test isolates cross-node routing
    // rather than artifact audience-manifest fan-out policy.
    helpers::upload_artifact(node0.artifact_port, &wasm_path, &sha256)
        .await
        .expect("failed to upload artifact to node 0");
    helpers::upload_artifact(node1.artifact_port, &wasm_path, &sha256)
        .await
        .expect("failed to upload artifact to node 1");
    helpers::deploy_app(
        &bus,
        app_id,
        admin_url(node0.artifact_port, &format!("/artifacts/{sha256}")),
        sha256.clone(),
        size_bytes,
        config.clone(),
    )
    .await
    .expect("failed to deploy hello app on node 0");
    helpers::deploy_app(
        &bus,
        app_id,
        admin_url(node1.artifact_port, &format!("/artifacts/{sha256}")),
        sha256.clone(),
        size_bytes,
        config,
    )
    .await
    .expect("failed to deploy hello app on node 1");
    cluster
        .add_route(host, app_id)
        .await
        .expect("failed to add hello route");

    assert_eq!(
        instance_count(&http, node0.admin_port, app_id)
            .await
            .expect("node 0 instance count"),
        0
    );
    assert_eq!(
        instance_count(&http, node1.admin_port, app_id)
            .await
            .expect("node 1 instance count"),
        0
    );

    bus.publish(&messaging::events::Event::NodeUnderPressure {
        node_id: node0.node_id.clone(),
        pressure_level: 2,
    })
    .await
    .expect("failed to publish overloaded local pressure event");
    bus.publish(&messaging::events::Event::NodeLoad {
        node_id: node1.node_id.clone(),
        cpu_percent: 5.0,
        fuel_budget_used_percent: 5.0,
        active_instances: 0,
        proxy_address: node1.proxy_addr_str(),
    })
    .await
    .expect("failed to publish remote load");

    tokio::time::sleep(Duration::from_millis(750)).await;

    let (status, body) = helpers::send_request_text(node0.proxy_port, host, "/")
        .await
        .expect("request through overloaded node 0 failed");
    assert_eq!(status, 200, "expected successful remote-routed response");
    assert!(
        body.contains("Hello"),
        "expected hello-axum response body, got: {body}"
    );

    let node0_instances = instance_count(&http, node0.admin_port, app_id)
        .await
        .expect("node 0 instance count after request");
    let node1_instances = instance_count(&http, node1.admin_port, app_id)
        .await
        .expect("node 1 instance count after request");

    assert_eq!(
        node0_instances, 0,
        "overloaded node 0 should proxy to node 1 instead of cold-starting locally"
    );
    assert_eq!(
        node1_instances, 1,
        "remote node 1 should cold-start the app behind the routed request"
    );
}

#[tokio::test]
#[ignore = "live single-node regression; run explicitly or via CI E2E lane"]
async fn test_live_hardened_instance_port_is_not_reachable_via_non_loopback_address() {
    let _guard = NODE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let cluster = ClusterFixture::single()
        .await
        .expect("failed to start single-node cluster fixture");
    let node = cluster.node(0);

    let app_id = "hello-hardened:v1";
    let host = "hello-hardened.local";
    let wasm_path = helpers::find_hello_axum_wasm().expect("hello-axum.wasm not found");

    cluster
        .deploy_app(app_id, &wasm_path)
        .await
        .expect("failed to deploy hello-axum");
    cluster
        .add_route(host, app_id)
        .await
        .expect("failed to add route");

    helpers::wait_for_app_ready(node.proxy_port, host, 40)
        .await
        .expect("app did not become ready through proxy");

    let (instance_port, direct_loopback) = wait_for_instance_port(node, Duration::from_secs(15))
        .await
        .expect("failed to find listening instance port");
    assert!(
        direct_loopback.status().is_success(),
        "loopback direct instance request should confirm the app is actually listening"
    );
    let direct_loopback_body = direct_loopback
        .text()
        .await
        .expect("failed to read direct loopback response body");
    assert!(
        direct_loopback_body.contains("Hello"),
        "expected hello-axum body from direct loopback instance port, got: {direct_loopback_body}"
    );

    let non_loopback_ip =
        local_non_loopback_ip().expect("failed to determine non-loopback local IP");
    let direct_non_loopback = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(format!("http://{non_loopback_ip}:{instance_port}/"))
        .send()
        .await;

    assert!(
        direct_non_loopback.is_err(),
        "instance port {instance_port} should not be reachable via non-loopback address {non_loopback_ip}"
    );
}
