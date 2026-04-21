//! L4 Chaos Test: Full Node Rebuild Recovery
//!
//! Verifies that when a node's entire redb database is deleted (simulating total
//! disk loss, hardware replacement, or catastrophic corruption), the node can
//! rebuild its state from the cluster by requesting a `StateSnapshot` from a
//! peer node.
//!
//! ## What This Tests
//!
//! - The `NodeJoined` event triggers the bootstrap protocol on peer nodes.
//! - Peer nodes respond with a `StateSnapshot` containing all app configs,
//!   routes, and encrypted secrets.
//! - The joining node reconstructs its redb from the snapshot.
//! - After rebuild, the node can cold-start apps and serve traffic.
//! - The node reconnects to NATS and resumes receiving events.
//!
//! ## TTR Target
//!
//! | Metric    | Target  | Max   |
//! |-----------|---------|-------|
//! | TTR (L4)  | under 120s  | 300s  |
//!
//! The TTR is measured from the moment the node is restarted with an empty
//! database to the moment it can serve traffic for a previously deployed app.
//!
//! ## WSL Requirement
//!
//! This test uses `SIGKILL` to stop the node process and file deletion to
//! remove the redb database. Both operations require a Unix-like environment.
//! It **must run inside WSL** or on a native Linux host.
//!
//! ## Why Two Nodes?
//!
//! This test requires at least two nodes because:
//!
//! 1. Node-1 (the "survivor") holds the cluster state in its redb and in
//!    JetStream. When node-0 restarts with an empty database, it publishes
//!    a `NodeJoined` event.
//! 2. Node-1 receives the `NodeJoined` event and responds with a
//!    `StateSnapshot` containing all app configs, routes, and encrypted
//!    secrets.
//! 3. Node-0 processes the `StateSnapshot` and reconstructs its redb.
//!
//! Without a survivor node, there would be no source of truth for the
//! cluster state, and the rebuild would fail.

use crate::fixture::ClusterFixture;
use crate::helpers;
use crate::injector;
use crate::reporter::{StepResult, TestReport};
use crate::verifier;
use std::time::Duration;
use tracing::info;

/// Test: Delete a node's redb entirely and verify it rebuilds from the cluster.
///
/// ## Steps
///
/// 1. **Setup**: Start a two-node cluster and deploy `chaos-app:v1` with a route
/// 2. **Verify**: Both nodes have the app and the route works
/// 3. **Stop**: Kill node-0 (the node that will lose its database)
/// 4. **Inject**: Delete node-0's redb file entirely (L4 failure)
/// 5. **Restart**: Start node-0 again with an empty database
/// 6. **Verify**: Node-0 becomes healthy (re-bootstrap from cluster)
/// 7. **Verify**: Node-0 received a `StateSnapshot` from node-1
/// 8. **Verify**: The route was restored on node-0
/// 9. **Verify**: The app can be cold-started on node-0
/// 10. **Verify**: The proxy serves traffic on node-0
///
/// ## Expected Behavior
///
/// When node-0 starts with an empty redb:
///
/// 1. The `detect_recovery_mode()` function returns `FullRebuild` (0 artifacts).
/// 2. The node publishes a `NodeJoined` event to NATS with its artifact
///    server URL and a one-time public key for secret transfer.
/// 3. Node-1 receives the `NodeJoined` event and responds with a
///    `StateSnapshot` containing:
///    - All app configs (JSON)
///    - All routes
///    - Encrypted secrets (encrypted with node-0's one-time public key)
///    - SHA-256 hashes of each app's .wasm artifact
/// 4. Node-0 processes the `StateSnapshot`:
///    - Saves app configs to redb
///    - Saves routes to redb and registers them in the host router
///    - Decrypts and stores secrets
///    - Fetches .wasm artifacts from node-1's artifact server
/// 5. After processing, node-0 has a complete copy of the cluster state.
/// 6. The node is healthy and can serve traffic.
///
/// The total TTR should be approximately:
///
/// ```text
/// TTR ≈ process_start + NodeJoined_publish + StateSnapshot_transfer +
///        artifact_fetch + cold_start
/// TTR ≈ 2s + 1s + 2-5s + 10-60s + 5-10s = 20-78s (target: under 120s, max: 300s)
/// ```
///
/// The artifact fetch time depends on the size of the .wasm files and
/// the network speed between the two nodes (localhost in this case).
pub async fn test_l4_full_rebuild_recovery() -> TestReport {
    let mut report = TestReport::new("L4: Full Node Rebuild Recovery");

    // ── Setup ──────────────────────────────────────────────────────
    // Two nodes: node-0 will lose its database, node-1 is the survivor
    let mut fixture = match ClusterFixture::dual().await {
        Ok(f) => f,
        Err(e) => {
            report.fail_setup(&format!("cluster setup failed: {e}"));
            return report;
        }
    };

    let app_id = "chaos-app:v1";
    let host = "chaos.local";

    // Extract owned addresses before any mutable operations on fixture
    let admin_addr_0 = fixture.node(0).admin_addr_str();
    let proxy_addr_0 = fixture.node(0).proxy_addr_str();
    let admin_addr_1 = fixture.node(1).admin_addr_str();
    let proxy_addr_1 = fixture.node(1).proxy_addr_str();
    let db_path = fixture.node(0).db_path.clone();

    // Deploy an app with a route
    let step = match helpers::setup_deploy_app(&fixture, app_id, host).await {
        Ok(_) => StepResult::pass("setup_deploy_app", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait for both nodes to have the app
    for i in 0..2 {
        let admin_addr = fixture.node(i).admin_addr_str();
        let step =
            match verifier::wait_for_app_instances(&admin_addr, app_id, 1, Duration::from_secs(30))
                .await
            {
                Ok(ttr) => StepResult::pass(
                    &format!("wait_for_app_node_{i}"),
                    &format!("{}ms", ttr.as_millis()),
                ),
                Err(e) => StepResult::fail(&format!("wait_for_app_node_{i}"), &e),
            };
        report.add_step(step);
    }

    if report.failed() {
        return report;
    }

    // ── Verify: The route works on both nodes ──────────────────────
    let step = match verifier::verify_route_exists(&admin_addr_0, host).await {
        Ok(_) => StepResult::pass("verify_route_node_0", "route exists"),
        Err(e) => StepResult::fail("verify_route_node_0", &e),
    };
    report.add_step(step);

    let step = match verifier::verify_route_exists(&admin_addr_1, host).await {
        Ok(_) => StepResult::pass("verify_route_node_1", "route exists"),
        Err(e) => StepResult::fail("verify_route_node_1", &e),
    };
    report.add_step(step);

    let step = match verifier::verify_proxy_request(&proxy_addr_0, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_traffic_node_0", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_traffic_node_0", &e),
    };
    report.add_step(step);

    let step = match verifier::verify_proxy_request(&proxy_addr_1, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_traffic_node_1", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_traffic_node_1", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Stop: Kill node-0 ──────────────────────────────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::inject_node_kill(fixture.node_mut(0)) {
        Ok(result) => StepResult::pass("stop_node_0", &result.description),
        Err(e) => StepResult::fail("stop_node_0", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait for the process to fully exit
    tokio::time::sleep(Duration::from_secs(2)).await;

    let step = if fixture.node_mut(0).is_running() {
        StepResult::fail("wait_for_exit", "node-0 is still running after SIGKILL")
    } else {
        StepResult::pass("wait_for_exit", "node-0 process exited")
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Inject: Delete node-0's redb file entirely ─────────────────
    // This simulates total disk loss — the node has no local state at all.
    // On restart, it will detect an empty database and enter FullRebuild
    // mode, requesting a StateSnapshot from node-1.
    let step = match std::fs::remove_file(&db_path) {
        Ok(_) => StepResult::pass(
            "delete_database",
            &format!("deleted redb at {}", db_path.display()),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File may have been cleaned up by the Drop impl — that's fine
            StepResult::pass(
                "delete_database",
                &format!("redb already absent at {}", db_path.display()),
            )
        }
        Err(e) => StepResult::fail("delete_database", &format!("failed to delete redb: {e}")),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Also clean up any auxiliary files (wal, lock) that redb may have created
    let step = {
        let db_dir = db_path.parent().unwrap_or(std::path::Path::new("/tmp"));
        let mut cleaned = false;
        if let Ok(read_dir) = std::fs::read_dir(db_dir) {
            for entry in read_dir.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                // Clean up redb auxiliary files (wal, lock) but leave the
                // temp directory and config file intact
                if name_str.starts_with("chaos_chaos-node-0.redb") {
                    let _ = std::fs::remove_file(entry.path());
                    cleaned = true;
                }
            }
        }
        if cleaned {
            StepResult::pass("cleanup_auxiliary_files", "cleaned up auxiliary redb files")
        } else {
            StepResult::pass("cleanup_auxiliary_files", "no auxiliary files found")
        }
    };
    report.add_step(step);

    // ── Restart: Start node-0 with an empty database ───────────────
    // The node will detect an empty database (0 artifacts) and enter
    // FullRebuild mode. It will publish a NodeJoined event and wait
    // for a StateSnapshot from node-1.
    let step = match fixture.node_mut(0).restart() {
        Ok(_) => StepResult::pass("restart_node_0", "ok"),
        Err(e) => StepResult::fail("restart_node_0", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: Node-0 becomes healthy ──────────────────────────────
    // The node needs time to:
    // 1. Start up and detect empty redb
    // 2. Publish NodeJoined event
    // 3. Receive StateSnapshot from node-1
    // 4. Process the snapshot (save configs, routes, secrets)
    // 5. Fetch artifacts from node-1's artifact server
    // 6. Reconnect to NATS
    //
    // This can take a while, especially the artifact fetch step.
    let step = match verifier::wait_for_node_healthy(&admin_addr_0, Duration::from_secs(120)).await
    {
        Ok(ttr) => StepResult::pass("wait_for_healthy", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("wait_for_healthy", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    if report.failed() {
        return report;
    }

    // ── Verify: Node-0 received a StateSnapshot from node-1 ────────
    // The StateSnapshot contains all app configs, routes, and encrypted
    // secrets. After processing, node-0 should have the route in its
    // redb and host router.
    //
    // We give the node extra time to process the snapshot and fetch
    // artifacts. The artifact fetch is the slowest part because it
    // involves downloading the .wasm file over HTTP.
    tokio::time::sleep(Duration::from_secs(10)).await;

    let step = match verifier::verify_route_exists(&admin_addr_0, host).await {
        Ok(_) => StepResult::pass("wait_for_state_snapshot", "route exists"),
        Err(e) => StepResult::fail("wait_for_state_snapshot", &e),
    };
    report.add_step(step);

    // ── Verify: The app can be cold-started on node-0 ───────────────
    // After the StateSnapshot is processed, node-0 has the app config
    // and artifact. A cold start (triggered by the first request) should
    // spawn a new instance.
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr_0, host).await;

    let step =
        match verifier::wait_for_app_instances(&admin_addr_0, app_id, 1, Duration::from_secs(60))
            .await
        {
            Ok(ttr) => StepResult::pass(
                "verify_app_on_rebuilt_node",
                &format!("{}ms", ttr.as_millis()),
            ),
            Err(e) => StepResult::fail("verify_app_on_rebuilt_node", &e),
        };
    report.add_step(step);

    // ── Verify: The proxy serves traffic on node-0 ──────────────────
    let step = match verifier::verify_proxy_request(&proxy_addr_0, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_traffic_served", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_traffic_served", &e),
    };
    report.add_step(step);

    // ── Verify: NATS is reconnected on node-0 ──────────────────────
    let step = match verifier::verify_nats_connected(&admin_addr_0, Duration::from_secs(10)).await {
        Ok(ttr) => StepResult::pass("verify_nats_reconnected", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_nats_reconnected", &e),
    };
    report.add_step(step);

    // ── Verify: Node-1 is still healthy (survivor) ──────────────────
    // Node-1 should not have been affected by node-0's database loss.
    let step = match verifier::wait_for_node_healthy(&admin_addr_1, Duration::from_secs(10)).await {
        Ok(ttr) => StepResult::pass("verify_node_1_healthy", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_node_1_healthy", &e),
    };
    report.add_step(step);

    // ── Verify: Node-1 still serves traffic ─────────────────────────
    let step = match verifier::verify_proxy_request(&proxy_addr_1, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_node_1_traffic", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_node_1_traffic", &e),
    };
    report.add_step(step);

    // ── Verify: Billing chain is intact on node-1 ──────────────────
    // Node-1's billing chain should be unaffected by node-0's rebuild.
    let step = match verifier::verify_billing_chain(&admin_addr_1).await {
        Ok(_) => StepResult::pass("verify_billing_chain_node_1", "valid"),
        Err(e) => StepResult::fail("verify_billing_chain_node_1", &e),
    };
    report.add_step(step);

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L4 chaos test PASSED — full node rebuild recovered"
        );
    }

    report
}

/// Variant: Test that a rebuilt node can receive new deployments after rebuild.
///
/// After a full rebuild, the node should be fully operational and able to
/// receive new deployment events from NATS. This test verifies that the
/// rebuilt node is not stuck in a degraded or read-only state.
///
/// ## Steps
///
/// 1. **Setup**: Start a two-node cluster and deploy an app
/// 2. **Rebuild**: Delete node-0's redb and restart (same as main test)
/// 3. **Verify**: Node-0 is healthy and has the original app
/// 4. **Deploy**: Deploy a second app via NATS
/// 5. **Verify**: Both nodes have the second app
/// 6. **Verify**: Node-0 serves traffic for the second app
pub async fn test_l4_rebuilt_node_receives_new_deployments() -> TestReport {
    let mut report = TestReport::new("L4: Rebuilt Node Receives New Deployments");

    // ── Setup ──────────────────────────────────────────────────────
    let mut fixture = match ClusterFixture::dual().await {
        Ok(f) => f,
        Err(e) => {
            report.fail_setup(&format!("cluster setup failed: {e}"));
            return report;
        }
    };

    let app_id_1 = "chaos-app:v1";
    let host_1 = "chaos.local";

    // Extract owned addresses before any mutable operations on fixture
    let admin_addr_0 = fixture.node(0).admin_addr_str();
    let proxy_addr_0 = fixture.node(0).proxy_addr_str();
    let _admin_addr_1 = fixture.node(1).admin_addr_str();
    let db_path = fixture.node(0).db_path.clone();

    // Deploy the first app
    let step = match helpers::setup_deploy_app(&fixture, app_id_1, host_1).await {
        Ok(_) => StepResult::pass("setup_deploy_app_1", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app_1", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait for both nodes
    for i in 0..2 {
        let admin_addr = fixture.node(i).admin_addr_str();
        let step = match verifier::wait_for_app_instances(
            &admin_addr,
            app_id_1,
            1,
            Duration::from_secs(30),
        )
        .await
        {
            Ok(ttr) => StepResult::pass(
                &format!("wait_for_app1_node_{i}"),
                &format!("{}ms", ttr.as_millis()),
            ),
            Err(e) => StepResult::fail(&format!("wait_for_app1_node_{i}"), &e),
        };
        report.add_step(step);
    }

    if report.failed() {
        return report;
    }

    // ── Rebuild: Delete node-0's redb and restart ──────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::inject_node_kill(fixture.node_mut(0)) {
        Ok(result) => StepResult::pass("stop_node_0", &result.description),
        Err(e) => StepResult::fail("stop_node_0", &e),
    };
    report.add_step(step);

    // Wait for exit
    tokio::time::sleep(Duration::from_secs(2)).await;

    let step = if fixture.node_mut(0).is_running() {
        StepResult::fail("wait_for_exit", "node-0 is still running after SIGKILL")
    } else {
        StepResult::pass("wait_for_exit", "node-0 process exited")
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Delete the database
    let step = match std::fs::remove_file(&db_path) {
        Ok(_) => StepResult::pass("delete_database", "deleted redb"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            StepResult::pass("delete_database", "redb already absent")
        }
        Err(e) => StepResult::fail("delete_database", &format!("failed to delete redb: {e}")),
    };
    report.add_step(step);

    // Restart node-0
    let step = match fixture.node_mut(0).restart() {
        Ok(_) => StepResult::pass("restart_node_0", "ok"),
        Err(e) => StepResult::fail("restart_node_0", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait for node-0 to become healthy
    let step = match verifier::wait_for_node_healthy(&admin_addr_0, Duration::from_secs(120)).await
    {
        Ok(ttr) => StepResult::pass("wait_for_healthy", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("wait_for_healthy", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    if report.failed() {
        return report;
    }

    // Wait for the StateSnapshot to be processed
    tokio::time::sleep(Duration::from_secs(10)).await;

    let step = match verifier::verify_route_exists(&admin_addr_0, host_1).await {
        Ok(_) => StepResult::pass("wait_for_snapshot", "route exists"),
        Err(e) => StepResult::fail("wait_for_snapshot", &e),
    };
    report.add_step(step);

    // ── Deploy: Deploy a second app via NATS ────────────────────────
    // This verifies that the rebuilt node is fully operational and can
    // receive new deployment events from NATS.
    let app_id_2 = "chaos-app-2:v1";
    let host_2 = "chaos-2.local";

    let step = match helpers::setup_deploy_app(&fixture, app_id_2, host_2).await {
        Ok(_) => StepResult::pass("deploy_app_2", "ok"),
        Err(e) => StepResult::fail("deploy_app_2", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: Both nodes have the second app ──────────────────────
    for i in 0..2 {
        let proxy_addr = fixture.node(i).proxy_addr_str();
        let admin_addr = fixture.node(i).admin_addr_str();

        // Trigger cold start
        let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host_2).await;

        let step = match verifier::wait_for_app_instances(
            &admin_addr,
            app_id_2,
            1,
            Duration::from_secs(30),
        )
        .await
        {
            Ok(ttr) => StepResult::pass(
                &format!("wait_for_app2_node_{i}"),
                &format!("{}ms", ttr.as_millis()),
            ),
            Err(e) => StepResult::fail(&format!("wait_for_app2_node_{i}"), &e),
        };
        report.add_step(step);
    }

    // ── Verify: Node-0 serves traffic for the second app ───────────
    let step = match verifier::verify_proxy_request(&proxy_addr_0, host_2, 200).await {
        Ok(ttr) => StepResult::pass(
            "verify_app2_traffic_node_0",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_app2_traffic_node_0", &e),
    };
    report.add_step(step);

    // ── Verify: Node-0 still serves traffic for the first app ──────
    let step = match verifier::verify_proxy_request(&proxy_addr_0, host_1, 200).await {
        Ok(ttr) => StepResult::pass(
            "verify_app1_traffic_node_0",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_app1_traffic_node_0", &e),
    };
    report.add_step(step);

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L4 rebuilt node receives new deployments — PASSED"
        );
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l4_report_name() {
        let report = TestReport::new("L4: Full Node Rebuild Recovery");
        assert_eq!(report.name, "L4: Full Node Rebuild Recovery");
        assert!(report.passed());
    }

    #[test]
    fn test_l4_report_step_sequence() {
        let mut report = TestReport::new("L4: Full Node Rebuild Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_app_node_0", "1 instance"));
        report.add_step(StepResult::pass("wait_for_app_node_1", "1 instance"));
        report.add_step(StepResult::pass("verify_route_node_0", "route exists"));
        report.add_step(StepResult::pass("verify_route_node_1", "route exists"));
        report.add_step(StepResult::pass("verify_traffic_node_0", "200 OK"));
        report.add_step(StepResult::pass("verify_traffic_node_1", "200 OK"));
        report.add_step(StepResult::pass("stop_node_0", "ok"));
        report.add_step(StepResult::pass("wait_for_exit", "node-0 process exited"));
        report.add_step(StepResult::pass(
            "delete_database",
            "deleted redb at /tmp/...",
        ));
        report.add_step(StepResult::pass("cleanup_auxiliary_files", "ok"));
        report.add_step(StepResult::pass("restart_node_0", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy", "45000ms"));
        report.add_step(StepResult::pass("wait_for_state_snapshot", "route exists"));
        report.add_step(StepResult::pass("verify_app_on_rebuilt_node", "1 instance"));
        report.add_step(StepResult::pass("verify_traffic_served", "200 OK"));
        report.add_step(StepResult::pass("verify_nats_reconnected", "ok"));
        report.add_step(StepResult::pass("verify_node_1_healthy", "ok"));
        report.add_step(StepResult::pass("verify_node_1_traffic", "200 OK"));
        report.add_step(StepResult::pass("verify_billing_chain_node_1", "valid"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 20);
    }

    #[test]
    fn test_l4_report_with_ttr() {
        let mut report = TestReport::new("L4: Full Node Rebuild Recovery");
        report.add_step(StepResult::pass("delete_database", "deleted"));
        report.set_ttr(Duration::from_millis(45000));

        assert_eq!(report.ttr_ms, Some(45000));
    }

    #[test]
    fn test_l4_report_database_delete_failed() {
        let mut report = TestReport::new("L4: Full Node Rebuild Recovery");
        report.add_step(StepResult::pass("stop_node_0", "ok"));
        report.add_step(StepResult::fail("delete_database", "permission denied"));

        assert!(report.failed());
    }

    #[test]
    fn test_l4_report_restart_failed() {
        let mut report = TestReport::new("L4: Full Node Rebuild Recovery");
        report.add_step(StepResult::pass("stop_node_0", "ok"));
        report.add_step(StepResult::pass("delete_database", "deleted"));
        report.add_step(StepResult::fail("restart_node_0", "binary not found"));

        assert!(report.failed());
    }

    #[test]
    fn test_l4_report_timeout() {
        let mut report = TestReport::new("L4: Full Node Rebuild Recovery");
        report.add_step(StepResult::pass("restart_node_0", "ok"));
        report.add_step(StepResult::fail(
            "wait_for_healthy",
            "node at 127.0.0.1:19090 did not become healthy within 120s",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l4_variant_report_name() {
        let report = TestReport::new("L4: Rebuilt Node Receives New Deployments");
        assert!(report.name.contains("Rebuilt"));
    }

    #[test]
    fn test_l4_variant_step_sequence() {
        let mut report = TestReport::new("L4: Rebuilt Node Receives New Deployments");
        report.add_step(StepResult::pass("setup_deploy_app_1", "ok"));
        report.add_step(StepResult::pass("wait_for_app1_node_0", "1 instance"));
        report.add_step(StepResult::pass("wait_for_app1_node_1", "1 instance"));
        report.add_step(StepResult::pass("stop_node_0", "ok"));
        report.add_step(StepResult::pass("wait_for_exit", "ok"));
        report.add_step(StepResult::pass("delete_database", "deleted"));
        report.add_step(StepResult::pass("restart_node_0", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy", "50000ms"));
        report.add_step(StepResult::pass("wait_for_snapshot", "route exists"));
        report.add_step(StepResult::pass("deploy_app_2", "ok"));
        report.add_step(StepResult::pass("wait_for_app2_node_0", "1 instance"));
        report.add_step(StepResult::pass("wait_for_app2_node_1", "1 instance"));
        report.add_step(StepResult::pass("verify_app2_traffic_node_0", "200 OK"));
        report.add_step(StepResult::pass("verify_app1_traffic_node_0", "200 OK"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 14);
    }

    #[test]
    fn test_l4_report_setup_failure() {
        let mut report = TestReport::new("L4: Full Node Rebuild Recovery");
        report.fail_setup("NATS container failed to start");

        assert!(report.failed());
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "setup");
    }

    #[test]
    fn test_l4_report_node_1_survives() {
        let mut report = TestReport::new("L4: Full Node Rebuild Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("stop_node_0", "ok"));
        report.add_step(StepResult::pass("delete_database", "deleted"));
        report.add_step(StepResult::pass("restart_node_0", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy", "ok"));
        report.add_step(StepResult::pass("verify_traffic_served", "200 OK"));
        report.add_step(StepResult::pass("verify_node_1_healthy", "ok"));
        report.add_step(StepResult::pass("verify_node_1_traffic", "200 OK"));

        assert!(report.passed());
    }
}
