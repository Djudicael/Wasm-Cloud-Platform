use common::artifact_transfer::{ArtifactManifestBatchRequest, ArtifactManifestBatchResponse};
use common::types::ClusterNodeRecord;
use e2e::fixture::ClusterFixture;
use e2e::helpers;
use std::time::{Duration, Instant};

static NODE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn admin_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
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
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| format!("failed to decode cluster registry response: {e}"))?;
                let nodes: Vec<ClusterNodeRecord> = serde_json::from_value(
                    body.get("nodes")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )
                .map_err(|e| format!("failed to deserialize cluster registry nodes: {e}"))?;

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

#[tokio::test]
#[ignore = "requires NATS + wasm-node binary + live two-node cluster in WSL"]
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
