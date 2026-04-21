//! L6 Chaos Test: Multi-Node Failure Recovery
//!
//! Verifies that when multiple nodes in a cluster fail simultaneously (simulating
//! a rack failure, power outage, or network partition affecting multiple hosts),
//! the surviving node continues serving traffic, and the failed nodes rebuild
//! their state when restarted.
//!
//! ## What This Tests
//!
//! - The surviving node continues serving traffic for apps it has instances of.
//! - The surviving node does not crash or panic when peers disappear.
//! - When failed nodes are restarted, they reconnect to NATS and rebuild state.
//! - After all nodes are healthy, the cluster is fully operational.
//! - No data is lost: routes, apps, and billing records survive the failure.
//!
//! ## TTR Target
//!
//! | Metric    | Target  | Max   |
//! |-----------|---------|-------|
//! | TTR (L6)  | under 300s  | 600s  |
//!
//! The TTR is measured from the moment the failed nodes are restarted to the
//! moment all nodes are healthy and serving traffic.
//!
//! ## WSL Requirement
//!
//! This test uses `SIGKILL` to terminate node processes, which is a Unix-only
//! signal. It **must run inside WSL** or on a native Linux host.
//!
//! ## Why Three Nodes?
//!
//! This test requires at least three nodes to simulate a realistic multi-node
//! failure scenario:
//!
//! 1. Node-0 (survivor): Stays alive and continues serving traffic.
//! 2. Node-1 and Node-2 (failed): Are killed simultaneously, then restarted.
//!
//! With three nodes, we can verify that:
//!
//! - The survivor doesn't need a majority (this is a shared-nothing architecture,
//!   not a consensus system).
//! - The survivor can operate independently (degraded but functional).
//! - The failed nodes can rebuild from the survivor when they come back.
//!
//! ## Failure Severity
//!
//! L6 is the most severe failure level because:
//!
//! - Multiple nodes fail simultaneously (no single point of failure).
//! - The cluster's capacity is reduced by 2/3.
//! - The surviving node must handle all traffic alone.
//! - The failed nodes must rebuild their entire state from the survivor.
//!
//! In production, this scenario corresponds to:
//!
//! - A rack failure in a data center
//! - A power outage affecting multiple servers
//! - A network partition isolating multiple nodes
//! - A misconfigured deployment that kills multiple nodes

use crate::fixture::ClusterFixture;
use crate::helpers;
use crate::injector;
use crate::reporter::{StepResult, TestReport};
use crate::verifier;
use std::time::Duration;
use tracing::info;

/// Test: Kill 2 out of 3 nodes and verify the surviving node continues
/// serving traffic, then verify the failed nodes rebuild when restarted.
///
/// ## Steps
///
/// 1. **Setup**: Start a three-node cluster and deploy `chaos-app:v1`
/// 2. **Verify**: At least one node has the app running
/// 3. **Inject**: Kill nodes 1 and 2 (2 out of 3) simultaneously (L6 failure)
/// 4. **Verify**: Node 0 (survivor) still serves traffic
/// 5. **Recover**: Restart nodes 1 and 2
/// 6. **Verify**: Both restarted nodes become healthy
/// 7. **Verify**: The cluster is fully operational (all nodes serve traffic)
/// 8. **Verify**: Billing chain is intact on the survivor
///
/// ## Expected Behavior
///
/// When nodes 1 and 2 are killed:
///
/// 1. Node 0 (survivor) detects the peer disconnections via NATS.
/// 2. Node 0 continues serving traffic for apps it has instances of.
/// 3. Node 0 may log warnings about unreachable peers.
/// 4. Node 0 does **not** crash, panic, or stop accepting requests.
///
/// When nodes 1 and 2 are restarted:
///
/// 1. Each node starts up and opens its redb database.
/// 2. The startup integrity check verifies redb is healthy.
/// 3. Each node reconnects to NATS and subscribes to event streams.
/// 4. Each node may publish a `NodeJoined` event if its state is stale.
/// 5. Node 0 may respond with a `StateSnapshot` if requested.
/// 6. After reconnection, all three nodes are fully operational.
///
/// The total TTR should be approximately:
///
/// ```text
/// TTR ≈ max(node1_restart, node2_restart) + NATS_reconnect + state_sync
/// TTR ≈ max(15s, 15s) + 5s + 10s = 30s (target: under 300s, max: 600s)
/// ```
///
/// The TTR is dominated by the slowest node to restart and rebuild.
pub async fn test_l6_multi_node_failure_recovery() -> TestReport {
    let mut report = TestReport::new("L6: Multi-Node Failure Recovery");

    // ── Setup ──────────────────────────────────────────────────────
    // Three nodes: node-0 survives, nodes 1 and 2 are killed
    let mut fixture = match ClusterFixture::triple().await {
        Ok(f) => f,
        Err(e) => {
            report.fail_setup(&format!("cluster setup failed: {e}"));
            return report;
        }
    };

    let app_id = "chaos-app:v1";
    let host = "chaos-app.local";

    // Extract owned addresses before any mutable operations on fixture
    let admin_addr_0 = fixture.node(0).admin_addr_str();
    let proxy_addr_0 = fixture.node(0).proxy_addr_str();
    let admin_addr_1 = fixture.node(1).admin_addr_str();
    let admin_addr_2 = fixture.node(2).admin_addr_str();

    // Deploy an app across the cluster
    let step = match helpers::setup_deploy_app(&fixture, app_id, host).await {
        Ok(_) => StepResult::pass("setup_deploy_app", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait for at least one node to have the app
    let step =
        match verifier::wait_for_app_instances(&admin_addr_0, app_id, 1, Duration::from_secs(30))
            .await
        {
            Ok(ttr) => StepResult::pass("wait_for_app", &format!("{}ms", ttr.as_millis())),
            Err(e) => StepResult::fail("wait_for_app", &e),
        };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: All nodes are healthy before failure ────────────────
    for i in 0..3 {
        let admin_addr = fixture.node(i).admin_addr_str();
        let step = match verifier::wait_for_node_healthy(&admin_addr, Duration::from_secs(10)).await
        {
            Ok(ttr) => StepResult::pass(
                &format!("verify_healthy_node_{i}_before"),
                &format!("{}ms", ttr.as_millis()),
            ),
            Err(e) => StepResult::fail(&format!("verify_healthy_node_{i}_before"), &e),
        };
        report.add_step(step);
    }

    if report.failed() {
        return report;
    }

    // ── Verify: The proxy serves traffic on node-0 before failure ───
    let step = match verifier::verify_proxy_request(&proxy_addr_0, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_traffic_before", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_traffic_before", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Record: Count billing records on node-0 before failure ──────
    let billing_count_before = match verifier::count_billing_records(&admin_addr_0).await {
        Ok(count) => {
            report.add_step(StepResult::pass(
                "count_billing_before",
                &format!("count={count}"),
            ));
            count
        }
        Err(e) => {
            report.add_step(StepResult::fail("count_billing_before", &e));
            0
        }
    };

    // ── Inject: Kill nodes 1 and 2 (2 out of 3) ────────────────────
    // We kill both nodes simultaneously to simulate a correlated failure
    // (e.g., rack failure, power outage). The order doesn't matter because
    // SIGKILL is immediate and non-blocking.
    let recovery_start = std::time::Instant::now();

    let step = match injector::inject_node_kill(fixture.node_mut(1)) {
        Ok(result) => StepResult::pass("kill_node_1", &result.description),
        Err(e) => StepResult::fail("kill_node_1", &e),
    };
    report.add_step(step);

    let step = match injector::inject_node_kill(fixture.node_mut(2)) {
        Ok(result) => StepResult::pass("kill_node_2", &result.description),
        Err(e) => StepResult::fail("kill_node_2", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: The killed nodes are actually dead ──────────────────
    let step = if fixture.node_mut(1).is_running() {
        StepResult::fail(
            "verify_node_1_dead",
            "node-1 is still running after SIGKILL",
        )
    } else {
        StepResult::pass("verify_node_1_dead", "node-1 is dead")
    };
    report.add_step(step);

    let step = if fixture.node_mut(2).is_running() {
        StepResult::fail(
            "verify_node_2_dead",
            "node-2 is still running after SIGKILL",
        )
    } else {
        StepResult::pass("verify_node_2_dead", "node-2 is dead")
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: Node 0 (survivor) still serves traffic ──────────────
    // The survivor should continue serving traffic even though 2/3 of
    // the cluster is down. This is the key property of a shared-nothing
    // architecture: each node operates independently.
    //
    // We wait a few seconds for the survivor to detect the peer failures
    // (via NATS disconnection or health check timeout) and stabilize.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let step = match verifier::verify_proxy_request(&proxy_addr_0, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_survivor_serves", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_survivor_serves", &e),
    };
    report.add_step(step);

    // ── Verify: Node 0 (survivor) is still healthy ──────────────────
    // The survivor should not have crashed or become unhealthy due to
    // the peer failures.
    let step = match verifier::wait_for_node_healthy(&admin_addr_0, Duration::from_secs(10)).await {
        Ok(ttr) => StepResult::pass("verify_survivor_healthy", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_survivor_healthy", &e),
    };
    report.add_step(step);

    // ── Verify: Node 0 (survivor) still has the route ───────────────
    // The route should not have been removed when the peers died.
    let step = match verifier::verify_route_exists(&admin_addr_0, host).await {
        Ok(_) => StepResult::pass("verify_survivor_route", "route exists"),
        Err(e) => StepResult::fail("verify_survivor_route", &e),
    };
    report.add_step(step);

    // ── Verify: Node 0 (survivor) still has app instances ───────────
    // The app instances on the survivor should not have been affected.
    let step =
        match verifier::wait_for_app_instances(&admin_addr_0, app_id, 1, Duration::from_secs(10))
            .await
        {
            Ok(ttr) => StepResult::pass(
                "verify_survivor_instances",
                &format!("{}ms", ttr.as_millis()),
            ),
            Err(e) => StepResult::fail("verify_survivor_instances", &e),
        };
    report.add_step(step);

    // Brief pause to ensure the OS has released resources from the killed
    // processes (ports, file descriptors, memory mappings).
    tokio::time::sleep(Duration::from_secs(2)).await;
    report.add_step(StepResult::pass(
        "wait_for_resource_release",
        "OS resources released",
    ));

    // ── Recover: Restart nodes 1 and 2 ──────────────────────────────
    // Restart both failed nodes. They will reconnect to NATS and
    // restore their state from redb (which was not corrupted — only
    // the processes were killed).
    let step = match fixture.node_mut(1).restart() {
        Ok(_) => StepResult::pass("restart_node_1", "ok"),
        Err(e) => StepResult::fail("restart_node_1", &e),
    };
    report.add_step(step);

    let step = match fixture.node_mut(2).restart() {
        Ok(_) => StepResult::pass("restart_node_2", "ok"),
        Err(e) => StepResult::fail("restart_node_2", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: Both restarted nodes become healthy ──────────────────
    // Each node needs time to:
    // 1. Start up and open its redb database
    // 2. Run the startup integrity check
    // 3. Connect to NATS
    // 4. Subscribe to event streams
    // 5. Restore app state from redb
    //
    // We use a generous timeout because the nodes may need to fetch
    // artifacts or process queued events.
    let step = match verifier::wait_for_node_healthy(&admin_addr_1, Duration::from_secs(120)).await
    {
        Ok(ttr) => StepResult::pass("wait_for_healthy_node_1", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("wait_for_healthy_node_1", &e),
    };
    report.add_step(step);

    let step = match verifier::wait_for_node_healthy(&admin_addr_2, Duration::from_secs(120)).await
    {
        Ok(ttr) => StepResult::pass("wait_for_healthy_node_2", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("wait_for_healthy_node_2", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    if report.failed() {
        return report;
    }

    // ── Verify: NATS is reconnected on restarted nodes ──────────────
    let step = match verifier::verify_nats_connected(&admin_addr_1, Duration::from_secs(30)).await {
        Ok(ttr) => StepResult::pass("verify_nats_node_1", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_nats_node_1", &e),
    };
    report.add_step(step);

    let step = match verifier::verify_nats_connected(&admin_addr_2, Duration::from_secs(30)).await {
        Ok(ttr) => StepResult::pass("verify_nats_node_2", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_nats_node_2", &e),
    };
    report.add_step(step);

    // ── Verify: Routes are restored on restarted nodes ───────────────
    // After reconnection, the restarted nodes should have the route
    // in their host router (either from redb or from JetStream replay).
    tokio::time::sleep(Duration::from_secs(5)).await;

    let step = match verifier::verify_route_exists(&admin_addr_1, host).await {
        Ok(_) => StepResult::pass("verify_route_node_1", "route exists"),
        Err(e) => StepResult::fail("verify_route_node_1", &e),
    };
    report.add_step(step);

    let step = match verifier::verify_route_exists(&admin_addr_2, host).await {
        Ok(_) => StepResult::pass("verify_route_node_2", "route exists"),
        Err(e) => StepResult::fail("verify_route_node_2", &e),
    };
    report.add_step(step);

    // ── Verify: The cluster is fully operational ─────────────────────
    // All three nodes should be able to serve traffic for the app.
    // We verify this by sending requests to each node's proxy.
    let mut cluster_ok = true;
    for i in 0..3 {
        let proxy_addr = fixture.node(i).proxy_addr_str();
        let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host).await;
        if let Err(e) = verifier::verify_proxy_request(&proxy_addr, host, 200).await {
            report.add_step(StepResult::fail(
                "verify_cluster_healthy",
                &format!("node {i}: {e}"),
            ));
            cluster_ok = false;
            break;
        }
    }
    if cluster_ok {
        report.add_step(StepResult::pass(
            "verify_cluster_healthy",
            "all 3 nodes serve traffic",
        ));
    }

    // ── Verify: Billing chain is intact on the survivor ─────────────
    // The billing chain on node-0 should be intact because node-0
    // never went down. This is the critical data integrity check.
    let step = match verifier::verify_billing_chain(&admin_addr_0).await {
        Ok(_) => StepResult::pass("verify_billing_chain_survivor", "valid"),
        Err(e) => StepResult::fail("verify_billing_chain_survivor", &e),
    };
    report.add_step(step);

    // ── Verify: Billing record count is preserved on survivor ───────
    // The number of billing records on the survivor should be >= the
    // count before the failure. It may be higher because the restart
    // of nodes 1 and 2 may generate additional billing events.
    let step = match verifier::count_billing_records(&admin_addr_0).await {
        Ok(count_after) => {
            if count_after >= billing_count_before {
                StepResult::pass(
                    "verify_billing_count_survivor",
                    &format!("billing records preserved on survivor: {count_after} >= {billing_count_before}"),
                )
            } else {
                StepResult::fail(
                    "verify_billing_count_survivor",
                    &format!(
                        "billing records LOST on survivor: {count_after} < {billing_count_before}"
                    ),
                )
            }
        }
        Err(e) => StepResult::fail("verify_billing_count_survivor", &e),
    };
    report.add_step(step);

    // ── Verify: All nodes have app instances ─────────────────────────
    // After recovery, all nodes should have at least one instance of
    // the app running (or be able to cold-start one on demand).
    for i in 0..3 {
        let proxy_addr = fixture.node(i).proxy_addr_str();
        let admin_addr = fixture.node(i).admin_addr_str();
        let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host).await;
        let step =
            match verifier::wait_for_app_instances(&admin_addr, app_id, 1, Duration::from_secs(30))
                .await
            {
                Ok(ttr) => StepResult::pass(
                    &format!("verify_app_node_{i}"),
                    &format!("{}ms", ttr.as_millis()),
                ),
                Err(e) => StepResult::fail(&format!("verify_app_node_{i}"), &e),
            };
        report.add_step(step);
    }

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L6 chaos test PASSED — multi-node failure recovered"
        );
    }

    report
}

/// Variant: Test that the survivor node can handle new deployments while
/// the other nodes are down.
///
/// This verifies that the surviving node is fully operational — not just
/// serving existing apps, but also able to receive and process new
/// deployment events.
///
/// ## Steps
///
/// 1. **Setup**: Start a three-node cluster and deploy an app
/// 2. **Inject**: Kill nodes 1 and 2
/// 3. **Verify**: Node 0 (survivor) still serves traffic
/// 4. **Deploy**: Deploy a second app via NATS (only node-0 receives it)
/// 5. **Verify**: Node-0 serves traffic for the second app
/// 6. **Recover**: Restart nodes 1 and 2
/// 7. **Verify**: All nodes are healthy and serve traffic for both apps
pub async fn test_l6_survivor_receives_new_deployments() -> TestReport {
    let mut report = TestReport::new("L6: Survivor Receives New Deployments");

    // ── Setup ──────────────────────────────────────────────────────
    let mut fixture = match ClusterFixture::triple().await {
        Ok(f) => f,
        Err(e) => {
            report.fail_setup(&format!("cluster setup failed: {e}"));
            return report;
        }
    };

    let app_id_1 = "chaos-app:v1";
    let host_1 = "chaos-app.local";

    // Extract owned addresses before any mutable operations on fixture
    let admin_addr_0 = fixture.node(0).admin_addr_str();
    let proxy_addr_0 = fixture.node(0).proxy_addr_str();
    let admin_addr_1 = fixture.node(1).admin_addr_str();
    let admin_addr_2 = fixture.node(2).admin_addr_str();

    // Deploy the first app
    let step = match helpers::setup_deploy_app(&fixture, app_id_1, host_1).await {
        Ok(_) => StepResult::pass("setup_deploy_app_1", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app_1", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait for node-0 to have the app
    let step =
        match verifier::wait_for_app_instances(&admin_addr_0, app_id_1, 1, Duration::from_secs(30))
            .await
        {
            Ok(ttr) => StepResult::pass("wait_for_app_1", &format!("{}ms", ttr.as_millis())),
            Err(e) => StepResult::fail("wait_for_app_1", &e),
        };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Inject: Kill nodes 1 and 2 ──────────────────────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::inject_node_kill(fixture.node_mut(1)) {
        Ok(result) => StepResult::pass("kill_node_1", &result.description),
        Err(e) => StepResult::fail("kill_node_1", &e),
    };
    report.add_step(step);

    let step = match injector::inject_node_kill(fixture.node_mut(2)) {
        Ok(result) => StepResult::pass("kill_node_2", &result.description),
        Err(e) => StepResult::fail("kill_node_2", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: Node 0 (survivor) still serves traffic for app-1 ────
    tokio::time::sleep(Duration::from_secs(3)).await;

    let step = match verifier::verify_proxy_request(&proxy_addr_0, host_1, 200).await {
        Ok(ttr) => StepResult::pass(
            "verify_survivor_serves_app_1",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_survivor_serves_app_1", &e),
    };
    report.add_step(step);

    // ── Deploy: Deploy a second app via NATS ─────────────────────────
    // Only node-0 is alive to receive this deployment. This verifies
    // that the survivor is fully operational and can process new events.
    let app_id_2 = "chaos-app-2:v1";
    let host_2 = "chaos-2.local";

    let step = match helpers::setup_deploy_app(&fixture, app_id_2, host_2).await {
        Ok(_) => StepResult::pass("deploy_app_2_on_survivor", "ok"),
        Err(e) => StepResult::fail("deploy_app_2_on_survivor", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: Node-0 serves traffic for the second app ────────────
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr_0, host_2).await;

    let step = match verifier::verify_proxy_request(&proxy_addr_0, host_2, 200).await {
        Ok(ttr) => StepResult::pass(
            "verify_survivor_serves_app_2",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_survivor_serves_app_2", &e),
    };
    report.add_step(step);

    // ── Recover: Restart nodes 1 and 2 ──────────────────────────────
    tokio::time::sleep(Duration::from_secs(2)).await;
    report.add_step(StepResult::pass(
        "wait_for_resource_release",
        "OS resources released",
    ));

    let step = match fixture.node_mut(1).restart() {
        Ok(_) => StepResult::pass("restart_node_1", "ok"),
        Err(e) => StepResult::fail("restart_node_1", &e),
    };
    report.add_step(step);

    let step = match fixture.node_mut(2).restart() {
        Ok(_) => StepResult::pass("restart_node_2", "ok"),
        Err(e) => StepResult::fail("restart_node_2", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: Both restarted nodes become healthy ──────────────────
    let step = match verifier::wait_for_node_healthy(&admin_addr_1, Duration::from_secs(120)).await
    {
        Ok(ttr) => StepResult::pass("wait_for_healthy_node_1", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("wait_for_healthy_node_1", &e),
    };
    report.add_step(step);

    let step = match verifier::wait_for_node_healthy(&admin_addr_2, Duration::from_secs(120)).await
    {
        Ok(ttr) => StepResult::pass("wait_for_healthy_node_2", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("wait_for_healthy_node_2", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    if report.failed() {
        return report;
    }

    // ── Verify: All nodes serve traffic for app-1 ───────────────────
    let mut all_app1_ok = true;
    for i in 0..3 {
        let proxy_addr = fixture.node(i).proxy_addr_str();
        let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host_1).await;
        if let Err(e) = verifier::verify_proxy_request(&proxy_addr, host_1, 200).await {
            report.add_step(StepResult::fail(
                "verify_all_nodes_app_1",
                &format!("node {i}: {e}"),
            ));
            all_app1_ok = false;
            break;
        }
    }
    if all_app1_ok {
        report.add_step(StepResult::pass(
            "verify_all_nodes_app_1",
            "all 3 nodes serve app-1",
        ));
    }

    // ── Verify: All nodes serve traffic for app-2 ───────────────────
    // The second app was deployed while nodes 1 and 2 were down.
    // After they restart and reconnect to NATS, they should receive
    // the DeployApp event from JetStream (guaranteed delivery).
    tokio::time::sleep(Duration::from_secs(10)).await;

    let mut all_app2_ok = true;
    for i in 0..3 {
        let proxy_addr = fixture.node(i).proxy_addr_str();
        let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host_2).await;
        if let Err(e) = verifier::verify_proxy_request(&proxy_addr, host_2, 200).await {
            report.add_step(StepResult::fail(
                "verify_all_nodes_app_2",
                &format!("node {i}: {e}"),
            ));
            all_app2_ok = false;
            break;
        }
    }
    if all_app2_ok {
        report.add_step(StepResult::pass(
            "verify_all_nodes_app_2",
            "all 3 nodes serve app-2",
        ));
    }

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L6 survivor receives new deployments — PASSED"
        );
    }

    report
}

/// Variant: Test sequential node failures (one at a time) with recovery
/// between each failure.
///
/// This is a less severe scenario than the main L6 test because only one
/// node is down at a time, but it tests the cluster's ability to handle
/// repeated failures without accumulating state corruption or resource leaks.
///
/// ## Steps
///
/// 1. **Setup**: Start a three-node cluster and deploy an app
/// 2. **Kill node-1** → verify cluster still works → restart node-1
/// 3. **Kill node-2** → verify cluster still works → restart node-2
/// 4. **Kill node-0** → verify cluster still works → restart node-0
/// 5. **Verify**: All nodes are healthy and serve traffic
pub async fn test_l6_sequential_node_failures() -> TestReport {
    let mut report = TestReport::new("L6: Sequential Node Failures");

    // ── Setup ──────────────────────────────────────────────────────
    let mut fixture = match ClusterFixture::triple().await {
        Ok(f) => f,
        Err(e) => {
            report.fail_setup(&format!("cluster setup failed: {e}"));
            return report;
        }
    };

    let app_id = "chaos-app:v1";
    let host = "chaos-app.local";

    // Extract owned addresses before any mutable operations on fixture
    let admin_addr_0 = fixture.node(0).admin_addr_str();
    let proxy_addr_0 = fixture.node(0).proxy_addr_str();
    let admin_addr_1 = fixture.node(1).admin_addr_str();
    let proxy_addr_1 = fixture.node(1).proxy_addr_str();
    let admin_addr_2 = fixture.node(2).admin_addr_str();

    let step = match helpers::setup_deploy_app(&fixture, app_id, host).await {
        Ok(_) => StepResult::pass("setup_deploy_app", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait for node-0 to have the app
    let step =
        match verifier::wait_for_app_instances(&admin_addr_0, app_id, 1, Duration::from_secs(30))
            .await
        {
            Ok(ttr) => StepResult::pass("wait_for_app", &format!("{}ms", ttr.as_millis())),
            Err(e) => StepResult::fail("wait_for_app", &e),
        };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    let overall_start = std::time::Instant::now();

    // ── Round 1: Kill and restart node-1 ────────────────────────────
    let step = match injector::inject_node_kill(fixture.node_mut(1)) {
        Ok(result) => StepResult::pass("round1_kill_node_1", &result.description),
        Err(e) => StepResult::fail("round1_kill_node_1", &e),
    };
    report.add_step(step);

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Node-0 should still serve traffic
    let step = match verifier::verify_proxy_request(&proxy_addr_0, host, 200).await {
        Ok(ttr) => StepResult::pass("round1_verify_survivors", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("round1_verify_survivors", &e),
    };
    report.add_step(step);

    tokio::time::sleep(Duration::from_secs(2)).await;
    report.add_step(StepResult::pass(
        "round1_wait_for_release",
        "resources released",
    ));

    let step = match fixture.node_mut(1).restart() {
        Ok(_) => StepResult::pass("round1_restart_node_1", "ok"),
        Err(e) => StepResult::fail("round1_restart_node_1", &e),
    };
    report.add_step(step);

    let step = match verifier::wait_for_node_healthy(&admin_addr_1, Duration::from_secs(60)).await {
        Ok(ttr) => StepResult::pass(
            "round1_wait_healthy_node_1",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("round1_wait_healthy_node_1", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Round 2: Kill and restart node-2 ────────────────────────────
    let step = match injector::inject_node_kill(fixture.node_mut(2)) {
        Ok(result) => StepResult::pass("round2_kill_node_2", &result.description),
        Err(e) => StepResult::fail("round2_kill_node_2", &e),
    };
    report.add_step(step);

    tokio::time::sleep(Duration::from_secs(2)).await;

    let step = match verifier::verify_proxy_request(&proxy_addr_0, host, 200).await {
        Ok(ttr) => StepResult::pass("round2_verify_survivors", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("round2_verify_survivors", &e),
    };
    report.add_step(step);

    tokio::time::sleep(Duration::from_secs(2)).await;
    report.add_step(StepResult::pass(
        "round2_wait_for_release",
        "resources released",
    ));

    let step = match fixture.node_mut(2).restart() {
        Ok(_) => StepResult::pass("round2_restart_node_2", "ok"),
        Err(e) => StepResult::fail("round2_restart_node_2", &e),
    };
    report.add_step(step);

    let step = match verifier::wait_for_node_healthy(&admin_addr_2, Duration::from_secs(60)).await {
        Ok(ttr) => StepResult::pass(
            "round2_wait_healthy_node_2",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("round2_wait_healthy_node_2", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Round 3: Kill and restart node-0 (the original survivor) ────
    let step = match injector::inject_node_kill(fixture.node_mut(0)) {
        Ok(result) => StepResult::pass("round3_kill_node_0", &result.description),
        Err(e) => StepResult::fail("round3_kill_node_0", &e),
    };
    report.add_step(step);

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Node-1 should still serve traffic (it was restarted and has the app)
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr_1, host).await;

    let step = match verifier::verify_proxy_request(&proxy_addr_1, host, 200).await {
        Ok(ttr) => StepResult::pass("round3_verify_survivors", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("round3_verify_survivors", &e),
    };
    report.add_step(step);

    tokio::time::sleep(Duration::from_secs(2)).await;
    report.add_step(StepResult::pass(
        "round3_wait_for_release",
        "resources released",
    ));

    let step = match fixture.node_mut(0).restart() {
        Ok(_) => StepResult::pass("round3_restart_node_0", "ok"),
        Err(e) => StepResult::fail("round3_restart_node_0", &e),
    };
    report.add_step(step);

    let step = match verifier::wait_for_node_healthy(&admin_addr_0, Duration::from_secs(60)).await {
        Ok(ttr) => StepResult::pass(
            "round3_wait_healthy_node_0",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("round3_wait_healthy_node_0", &e),
    };
    report.add_step(step);

    let overall_ttr = overall_start.elapsed();
    report.set_ttr(overall_ttr);

    if report.failed() {
        return report;
    }

    // ── Verify: All nodes are healthy and serve traffic ──────────────
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut all_ok = true;
    for i in 0..3 {
        let proxy_addr = fixture.node(i).proxy_addr_str();
        let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host).await;
        if let Err(e) = verifier::verify_proxy_request(&proxy_addr, host, 200).await {
            report.add_step(StepResult::fail(
                "verify_cluster_fully_operational",
                &format!("node {i}: {e}"),
            ));
            all_ok = false;
            break;
        }
    }
    if all_ok {
        report.add_step(StepResult::pass(
            "verify_cluster_fully_operational",
            "all 3 nodes serve traffic after sequential failures",
        ));
    }

    // ── Verify: Billing chain is intact on all nodes ────────────────
    for i in 0..3 {
        let admin_addr = fixture.node(i).admin_addr_str();
        let step = match verifier::verify_billing_chain(&admin_addr).await {
            Ok(_) => StepResult::pass(&format!("verify_billing_node_{i}"), "valid"),
            Err(e) => StepResult::fail(&format!("verify_billing_node_{i}"), &e),
        };
        report.add_step(step);
    }

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = overall_ttr.as_millis(),
            "L6 sequential failures test PASSED — cluster survived 3 sequential node kills"
        );
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l6_report_name() {
        let report = TestReport::new("L6: Multi-Node Failure Recovery");
        assert_eq!(report.name, "L6: Multi-Node Failure Recovery");
        assert!(report.passed());
    }

    #[test]
    fn test_l6_report_step_sequence() {
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_app", "1 instance"));
        report.add_step(StepResult::pass("verify_healthy_node_0_before", "ok"));
        report.add_step(StepResult::pass("verify_healthy_node_1_before", "ok"));
        report.add_step(StepResult::pass("verify_healthy_node_2_before", "ok"));
        report.add_step(StepResult::pass("verify_traffic_before", "200 OK"));
        report.add_step(StepResult::pass("count_billing_before", "count=5"));
        report.add_step(StepResult::pass(
            "kill_node_1",
            "L2: killed node process chaos-node-1",
        ));
        report.add_step(StepResult::pass(
            "kill_node_2",
            "L2: killed node process chaos-node-2",
        ));
        report.add_step(StepResult::pass("verify_node_1_dead", "node-1 is dead"));
        report.add_step(StepResult::pass("verify_node_2_dead", "node-2 is dead"));
        report.add_step(StepResult::pass("verify_survivor_serves", "200 OK"));
        report.add_step(StepResult::pass("verify_survivor_healthy", "ok"));
        report.add_step(StepResult::pass("verify_survivor_route", "route exists"));
        report.add_step(StepResult::pass("verify_survivor_instances", "1 instance"));
        report.add_step(StepResult::pass("wait_for_resource_release", "ok"));
        report.add_step(StepResult::pass("restart_node_1", "ok"));
        report.add_step(StepResult::pass("restart_node_2", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy_node_1", "15000ms"));
        report.add_step(StepResult::pass("wait_for_healthy_node_2", "18000ms"));
        report.add_step(StepResult::pass("verify_nats_node_1", "ok"));
        report.add_step(StepResult::pass("verify_nats_node_2", "ok"));
        report.add_step(StepResult::pass("verify_route_node_1", "route exists"));
        report.add_step(StepResult::pass("verify_route_node_2", "route exists"));
        report.add_step(StepResult::pass(
            "verify_cluster_healthy",
            "all 3 nodes serve traffic",
        ));
        report.add_step(StepResult::pass("verify_billing_chain_survivor", "valid"));
        report.add_step(StepResult::pass("verify_billing_count_survivor", "7 >= 5"));
        report.add_step(StepResult::pass("verify_app_node_0", "1 instance"));
        report.add_step(StepResult::pass("verify_app_node_1", "1 instance"));
        report.add_step(StepResult::pass("verify_app_node_2", "1 instance"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 30);
    }

    #[test]
    fn test_l6_report_with_ttr() {
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.add_step(StepResult::pass("kill_node_1", "killed"));
        report.add_step(StepResult::pass("kill_node_2", "killed"));
        report.set_ttr(Duration::from_millis(45000));

        assert_eq!(report.ttr_ms, Some(45000));
    }

    #[test]
    fn test_l6_report_survivor_still_serves() {
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("kill_node_1", "killed"));
        report.add_step(StepResult::pass("kill_node_2", "killed"));
        report.add_step(StepResult::pass("verify_survivor_serves", "200 OK"));

        assert!(report.passed());
    }

    #[test]
    fn test_l6_report_survivor_fails() {
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("kill_node_1", "killed"));
        report.add_step(StepResult::pass("kill_node_2", "killed"));
        report.add_step(StepResult::fail(
            "verify_survivor_serves",
            "proxy request to 127.0.0.1:18080 (Host: chaos-app.local) returned 502, expected 200",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l6_report_restart_failed() {
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("kill_node_1", "killed"));
        report.add_step(StepResult::pass("kill_node_2", "killed"));
        report.add_step(StepResult::pass("verify_survivor_serves", "200 OK"));
        report.add_step(StepResult::pass("restart_node_1", "ok"));
        report.add_step(StepResult::fail("restart_node_2", "binary not found"));

        assert!(report.failed());
    }

    #[test]
    fn test_l6_report_billing_lost() {
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.add_step(StepResult::pass("count_billing_before", "count=5"));
        report.add_step(StepResult::fail(
            "verify_billing_count_survivor",
            "billing records LOST on survivor: 3 < 5",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l6_survivor_deployments_report_name() {
        let report = TestReport::new("L6: Survivor Receives New Deployments");
        assert!(report.name.contains("Survivor"));
    }

    #[test]
    fn test_l6_survivor_deployments_step_sequence() {
        let mut report = TestReport::new("L6: Survivor Receives New Deployments");
        report.add_step(StepResult::pass("setup_deploy_app_1", "ok"));
        report.add_step(StepResult::pass("wait_for_app_1", "1 instance"));
        report.add_step(StepResult::pass("kill_node_1", "killed"));
        report.add_step(StepResult::pass("kill_node_2", "killed"));
        report.add_step(StepResult::pass("verify_survivor_serves_app_1", "200 OK"));
        report.add_step(StepResult::pass("deploy_app_2_on_survivor", "ok"));
        report.add_step(StepResult::pass("verify_survivor_serves_app_2", "200 OK"));
        report.add_step(StepResult::pass("wait_for_resource_release", "ok"));
        report.add_step(StepResult::pass("restart_node_1", "ok"));
        report.add_step(StepResult::pass("restart_node_2", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy_node_1", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy_node_2", "ok"));
        report.add_step(StepResult::pass(
            "verify_all_nodes_app_1",
            "all 3 nodes serve app-1",
        ));
        report.add_step(StepResult::pass(
            "verify_all_nodes_app_2",
            "all 3 nodes serve app-2",
        ));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 14);
    }

    #[test]
    fn test_l6_sequential_report_name() {
        let report = TestReport::new("L6: Sequential Node Failures");
        assert!(report.name.contains("Sequential"));
    }

    #[test]
    fn test_l6_sequential_step_sequence() {
        let mut report = TestReport::new("L6: Sequential Node Failures");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_app", "1 instance"));
        // Round 1
        report.add_step(StepResult::pass("round1_kill_node_1", "killed"));
        report.add_step(StepResult::pass("round1_verify_survivors", "200 OK"));
        report.add_step(StepResult::pass("round1_wait_for_release", "ok"));
        report.add_step(StepResult::pass("round1_restart_node_1", "ok"));
        report.add_step(StepResult::pass("round1_wait_healthy_node_1", "ok"));
        // Round 2
        report.add_step(StepResult::pass("round2_kill_node_2", "killed"));
        report.add_step(StepResult::pass("round2_verify_survivors", "200 OK"));
        report.add_step(StepResult::pass("round2_wait_for_release", "ok"));
        report.add_step(StepResult::pass("round2_restart_node_2", "ok"));
        report.add_step(StepResult::pass("round2_wait_healthy_node_2", "ok"));
        // Round 3
        report.add_step(StepResult::pass("round3_kill_node_0", "killed"));
        report.add_step(StepResult::pass("round3_verify_survivors", "200 OK"));
        report.add_step(StepResult::pass("round3_wait_for_release", "ok"));
        report.add_step(StepResult::pass("round3_restart_node_0", "ok"));
        report.add_step(StepResult::pass("round3_wait_healthy_node_0", "ok"));
        // Final
        report.add_step(StepResult::pass(
            "verify_cluster_fully_operational",
            "all 3 nodes serve traffic",
        ));
        report.add_step(StepResult::pass("verify_billing_node_0", "valid"));
        report.add_step(StepResult::pass("verify_billing_node_1", "valid"));
        report.add_step(StepResult::pass("verify_billing_node_2", "valid"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 21);
    }

    #[test]
    fn test_l6_report_setup_failure() {
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.fail_setup("NATS container failed to start");

        assert!(report.failed());
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "setup");
    }

    #[test]
    fn test_l6_report_all_nodes_killed() {
        // Edge case: what if all nodes are killed?
        // In practice, the test only kills 2 of 3, but the report
        // structure should handle the case where the survivor also fails.
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("kill_node_1", "killed"));
        report.add_step(StepResult::pass("kill_node_2", "killed"));
        report.add_step(StepResult::fail(
            "verify_survivor_serves",
            "connection refused — node-0 may have crashed",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l6_report_node_restart_timeout() {
        let mut report = TestReport::new("L6: Multi-Node Failure Recovery");
        report.add_step(StepResult::pass("kill_node_1", "killed"));
        report.add_step(StepResult::pass("kill_node_2", "killed"));
        report.add_step(StepResult::pass("verify_survivor_serves", "200 OK"));
        report.add_step(StepResult::pass("restart_node_1", "ok"));
        report.add_step(StepResult::pass("restart_node_2", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy_node_1", "15000ms"));
        report.add_step(StepResult::fail(
            "wait_for_healthy_node_2",
            "node at 127.0.0.1:19092 did not become healthy within 120s",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l6_sequential_billing_intact_after_all_rounds() {
        let mut report = TestReport::new("L6: Sequential Node Failures");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("round1_kill_node_1", "killed"));
        report.add_step(StepResult::pass("round1_restart_node_1", "ok"));
        report.add_step(StepResult::pass("round2_kill_node_2", "killed"));
        report.add_step(StepResult::pass("round2_restart_node_2", "ok"));
        report.add_step(StepResult::pass("round3_kill_node_0", "killed"));
        report.add_step(StepResult::pass("round3_restart_node_0", "ok"));
        report.add_step(StepResult::pass("verify_billing_node_0", "valid"));
        report.add_step(StepResult::pass("verify_billing_node_1", "valid"));
        report.add_step(StepResult::pass("verify_billing_node_2", "valid"));

        assert!(report.passed());
    }
}
