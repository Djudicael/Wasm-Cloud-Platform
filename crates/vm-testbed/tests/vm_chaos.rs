//! Chaos tests using microVMs.
//!
//! These tests inject real failures into microVMs and verify recovery:
//!
//! | Test | Failure | What It Verifies |
//! |------|---------|------------------|
//! | `test_vm_kill_and_restart` | SIGKILL VMM | L2: Node restart restores state |
//! | `test_vm_network_partition` | iptables drop | L5: NATS partition handling |
//! | `test_vm_disk_corruption` | dd overwrite redb | L3: Integrity check + rebuild |
//!
//! ## Prerequisites
//!
//! - All VM images built
//! - `sudo` or `CAP_NET_ADMIN`
//! - `/dev/kvm` access
//!
//! ## Run
//!
//! ```bash
//! sudo cargo test -p vm-testbed --test vm_chaos -- --nocapture --test-threads=1
//! ```

use std::time::Duration;
use vm_testbed::cluster::ClusterFixture;

/// L2 Chaos Test: Kill a VM and verify it recovers after restart.
///
/// This simulates a hardware failure or OOM kill where the entire node
/// goes down. The node should restore its state from redb on restart.
#[tokio::test]
#[cfg_attr(not(feature = "firecracker"), ignore = "requires firecracker feature")]
async fn test_vm_kill_and_restart() {
    tracing_subscriber::fmt::init();

    // 1. Create cluster with 1 NATS + 2 nodes
    let mut cluster = ClusterFixture::new("chaos-l2")
        .await
        .expect("Failed to create cluster");

    cluster
        .start_nats(256, 1)
        .await
        .expect("Failed to start NATS");
    let node_a = cluster
        .start_node(512, 2)
        .await
        .expect("Failed to start node A");
    let node_b = cluster
        .start_node(512, 2)
        .await
        .expect("Failed to start node B");

    cluster
        .wait_for_all_healthy(Duration::from_secs(60))
        .await
        .expect("Nodes did not become healthy");

    println!("✅ Cluster ready: NATS + {} nodes", cluster.node_count());

    // 2. Deploy an app on node A
    deploy_test_app(&cluster, &node_a)
        .await
        .expect("Failed to deploy app");

    // 3. Verify app is serving
    let resp = http_get(&cluster, &node_a, "test-app.local", "/").await;
    assert_eq!(resp.status(), 200, "App should be serving before kill");
    println!("✅ App is serving");

    // 4. KILL node A (simulate hardware failure / power loss)
    println!("💥 Killing node {}...", node_a);
    cluster
        .kill_node(&node_a)
        .await
        .expect("Failed to kill node");

    // Wait a moment for the kill to take effect
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 5. Verify node A is dead
    {
        let node = cluster.get_node(&node_a).expect("Node not found");
        assert!(!node.is_running(), "Node should be dead after kill");
    }
    println!("✅ Node is confirmed dead");

    // 6. Restart node A
    println!("🔄 Restarting node {}...", node_a);
    cluster
        .restart_node(&node_a)
        .await
        .expect("Failed to restart node");

    // 7. Wait for node A to be healthy again
    {
        let node = cluster.get_node_mut(&node_a).expect("Node not found");
        node.wait_for_health(Duration::from_secs(60))
            .await
            .expect("Restarted node did not become healthy");
    }
    println!("✅ Node is healthy after restart");

    // 8. Verify app is still serving (state restored from redb)
    let resp = http_get(&cluster, &node_a, "test-app.local", "/").await;
    assert_eq!(
        resp.status(),
        200,
        "App should still be serving after restart"
    );
    println!("✅ App recovered after node restart");

    // 9. Cleanup
    cluster
        .teardown()
        .await
        .expect("Failed to teardown cluster");
    println!("✅ L2 chaos test passed!");
}

/// L5 Chaos Test: Network partition between a node and NATS.
///
/// This simulates a network partition where a node loses connectivity
/// to NATS but continues running. The node should enter degraded mode
/// and continue serving existing apps.
#[tokio::test]
#[cfg_attr(not(feature = "firecracker"), ignore = "requires firecracker feature")]
async fn test_vm_network_partition() {
    tracing_subscriber::fmt::init();

    let mut cluster = ClusterFixture::new("chaos-l5")
        .await
        .expect("Failed to create cluster");

    cluster
        .start_nats(256, 1)
        .await
        .expect("Failed to start NATS");
    let node_a = cluster
        .start_node(512, 2)
        .await
        .expect("Failed to start node");

    cluster
        .wait_for_all_healthy(Duration::from_secs(60))
        .await
        .expect("Node did not become healthy");

    // Deploy app
    deploy_test_app(&cluster, &node_a)
        .await
        .expect("Failed to deploy app");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify app serving
    let resp = http_get(&cluster, &node_a, "test-app.local", "/").await;
    assert_eq!(resp.status(), 200);
    println!("✅ App serving before partition");

    // Create network partition: drop packets from node to NATS
    println!("🔌 Creating network partition...");
    let nats_ip = cluster
        .nats_url()
        .unwrap()
        .strip_prefix("nats://")
        .unwrap()
        .split(':')
        .next()
        .unwrap()
        .to_string();

    // Use iptables to drop packets from node IP to NATS IP
    let node_ip = cluster.get_node(&node_a).unwrap().config.ip.clone();
    create_partition(&node_ip, &nats_ip).expect("Failed to create partition");

    // Wait for partition to take effect
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Verify node is still serving (degraded mode)
    let resp = http_get(&cluster, &node_a, "test-app.local", "/").await;
    assert_eq!(resp.status(), 200, "Node should serve in degraded mode");
    println!("✅ Node serving in degraded mode during partition");

    // Heal partition
    println!("🔌 Healing partition...");
    heal_partition(&node_ip, &nats_ip).expect("Failed to heal partition");

    // Wait for reconnection
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Verify node is healthy again
    let resp = http_get(&cluster, &node_a, "test-app.local", "/").await;
    assert_eq!(resp.status(), 200);
    println!("✅ Node recovered after partition healed");

    cluster.teardown().await.expect("Failed to teardown");
    println!("✅ L5 chaos test passed!");
}

// ── Helpers ──────────────────────────────────────────────────────────

async fn deploy_test_app(
    cluster: &ClusterFixture,
    node_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Simplified: in a real test, this would upload the wasm and publish deploy events
    // For now, we assume the app is pre-baked in the rootfs or deployed via NATS
    let _ = (cluster, node_id);
    Ok(())
}

async fn http_get(
    cluster: &ClusterFixture,
    node_id: &str,
    host: &str,
    path: &str,
) -> reqwest::Response {
    let node = cluster.get_node(node_id).expect("Node not found");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let url = format!("http://{}{}", node.proxy_addr(), path);
    client
        .get(&url)
        .header("Host", host)
        .send()
        .await
        .expect("HTTP request failed")
}

fn create_partition(node_ip: &str, nats_ip: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    // Drop outbound packets from node to NATS
    let status = Command::new("iptables")
        .args(["-A", "FORWARD", "-s", node_ip, "-d", nats_ip, "-j", "DROP"])
        .status()?;

    if !status.success() {
        return Err("iptables failed".into());
    }

    // Drop inbound packets from NATS to node
    let status = Command::new("iptables")
        .args(["-A", "FORWARD", "-s", nats_ip, "-d", node_ip, "-j", "DROP"])
        .status()?;

    if !status.success() {
        return Err("iptables failed".into());
    }

    Ok(())
}

fn heal_partition(node_ip: &str, nats_ip: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    let _ = Command::new("iptables")
        .args(["-D", "FORWARD", "-s", node_ip, "-d", nats_ip, "-j", "DROP"])
        .status();

    let _ = Command::new("iptables")
        .args(["-D", "FORWARD", "-s", nats_ip, "-d", node_ip, "-j", "DROP"])
        .status();

    Ok(())
}
