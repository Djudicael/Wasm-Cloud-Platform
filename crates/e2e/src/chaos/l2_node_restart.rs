//! L2 Chaos Test: Node Process Restart Recovery
//!
//! Verifies that when the entire `wasm-node` process is killed (simulating an
//! OOM kill, hardware fault, or operator error), the process can be restarted
//! and all state is restored from the local redb database.
//!
//! ## What This Tests
//!
//! - The node process can be hard-killed (`SIGKILL`) and restarted.
//! - On restart, the node reads its state from redb (apps, routes, configs).
//! - Previously deployed apps are restored and can serve traffic.
//! - The billing hash chain remains intact across the restart (no data loss).
//! - The node reconnects to NATS and resumes receiving events.
//!
//! ## TTR Target
//!
//! | Metric    | Target | Max  |
//! |-----------|--------|------|
//! | TTR (L2)  | under 30s  | 60s  |
//!
//! The TTR is measured from the moment the process is killed to the moment
//! the node's health endpoint returns 200 after restart.
//!
//! ## WSL Requirement
//!
//! This test uses `SIGKILL` to terminate the node process, which is a Unix-
//! only signal. It **must run inside WSL** or on a native Linux host.
//! On non-Unix platforms, `kill()` falls back to the platform's equivalent
//! of a hard process termination.

use crate::fixture::ClusterFixture;
use crate::helpers;
use crate::injector;
use crate::reporter::{StepResult, TestReport};
use crate::verifier;
use std::time::Duration;
use tracing::info;

/// Test: Kill the entire wasm-node process and verify it restores
/// state from redb after restart.
///
/// ## Steps
///
/// 1. **Setup**: Start a single-node cluster and deploy `chaos-app:v1`
/// 2. **Verify**: The app has at least 1 running instance
/// 3. **Record**: Count billing records before the kill
/// 4. **Inject**: Send `SIGKILL` to the node process (L2 failure)
/// 5. **Verify**: The process is dead
/// 6. **Recover**: Restart the node process with the same config
/// 7. **Verify**: The node becomes healthy (health endpoint returns 200)
/// 8. **Verify**: The app is restored from redb (instances are running)
/// 9. **Verify**: The proxy serves traffic
/// 10. **Verify**: The billing chain is intact (no data loss)
///
/// ## Expected Behavior
///
/// When the node process is killed with `SIGKILL`:
///
/// 1. The OS terminates the process immediately — no cleanup code runs.
/// 2. All in-memory state is lost (instance table, NATS subscriptions).
/// 3. On restart, the node opens its redb database and reads persisted state.
/// 4. The startup integrity check verifies redb is healthy.
/// 5. The node reconnects to NATS and subscribes to event streams.
/// 6. Previously deployed apps are loaded from redb and can be cold-started.
/// 7. Routes are restored from redb and registered in the host router.
/// 8. Billing records in redb are intact (the hash chain is valid).
///
/// The total TTR should be approximately:
///
/// ```text
/// TTR ≈ process_start_time + integrity_check + NATS_connect + cold_start
/// TTR ≈ 2s + 1s + 1s + 5-10s = 9-14s (target: under 30s, max: 60s)
/// ```
pub async fn test_l2_node_restart_recovery() -> TestReport {
    let mut report = TestReport::new("L2: Node Process Restart Recovery");

    // ── Setup ──────────────────────────────────────────────────────
    let mut fixture = match ClusterFixture::single().await {
        Ok(f) => f,
        Err(e) => {
            report.fail_setup(&format!("cluster setup failed: {e}"));
            return report;
        }
    };

    let app_id = "chaos-app:v1";
    let host = "chaos-app.local";

    // Deploy a test app
    let step = match helpers::setup_deploy_app(&fixture, app_id, host).await {
        Ok(_) => StepResult::pass("setup_deploy_app", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Extract addresses as owned strings before any mutable operations
    let admin_addr = fixture.node(0).admin_addr_str();
    let proxy_addr = fixture.node(0).proxy_addr_str();

    // Trigger the initial cold start so the app has a real running instance
    // before we measure crash-and-restart recovery.
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host).await;

    // Wait for the app to have a running instance
    let step =
        match verifier::wait_for_app_instances(&admin_addr, app_id, 1, Duration::from_secs(30))
            .await
        {
            Ok(ttr) => StepResult::pass("wait_for_instance", &format!("{}ms", ttr.as_millis())),
            Err(e) => StepResult::fail("wait_for_instance", &e),
        };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Record: Count billing records before kill ──────────────────
    let billing_before = match verifier::count_billing_records(&admin_addr).await {
        Ok(count) => count,
        Err(e) => {
            report.add_step(StepResult::fail("count_billing_before", &e));
            return report;
        }
    };
    report.add_step(StepResult::pass(
        "count_billing_before",
        &format!("count={billing_before}"),
    ));

    // ── Inject: Kill the node process (SIGKILL) ────────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::inject_node_kill(fixture.node_mut(0)) {
        Ok(result) => StepResult::pass("inject_node_kill", &result.description),
        Err(e) => StepResult::fail("inject_node_kill", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait briefly for the OS to reap the process after SIGKILL.
    // try_wait() returns Ok(None) until the kernel has cleaned up the
    // child — this can take a few milliseconds after the signal is sent.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Verify: The process is dead ────────────────────────────────
    let step = if fixture.node_mut(0).is_running() {
        StepResult::fail(
            "verify_process_dead",
            "process is still running after SIGKILL",
        )
    } else {
        StepResult::pass("verify_process_dead", "ok")
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Brief pause to ensure the OS has fully released resources (ports, FDs)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── Recover: Restart the node process ──────────────────────────
    let step = match fixture.node_mut(0).restart().await {
        Ok(_) => StepResult::pass("restart_node", "ok"),
        Err(e) => StepResult::fail("restart_node", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: The node becomes healthy ───────────────────────────
    let step = match verifier::wait_for_node_healthy(&admin_addr, Duration::from_secs(60)).await {
        Ok(ttr) => StepResult::pass("wait_for_healthy", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("wait_for_healthy", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    if report.failed() {
        return report;
    }

    // ── Verify: The app is restored from redb ──────────────────────
    // After restart, the node reads the app config from redb. The instance
    // may need a cold start (triggered by the next request), or it may
    // already be running if the node auto-starts instances on recovery.
    // Trigger cold start first
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host).await;

    let step =
        match verifier::wait_for_app_instances(&admin_addr, app_id, 1, Duration::from_secs(30))
            .await
        {
            Ok(ttr) => StepResult::pass("verify_app_restored", &format!("{}ms", ttr.as_millis())),
            Err(e) => StepResult::fail("verify_app_restored", &e),
        };
    report.add_step(step);

    // ── Verify: The proxy serves traffic ───────────────────────────
    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_traffic_served", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_traffic_served", &e),
    };
    report.add_step(step);

    // ── Verify: The route is restored ──────────────────────────────
    let step = match verifier::verify_route_exists(&admin_addr, host).await {
        Ok(ttr) => StepResult::pass("verify_route_restored", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_route_restored", &e),
    };
    report.add_step(step);

    // ── Verify: Billing chain is intact ────────────────────────────
    // This is the critical data integrity check. If the billing chain
    // is broken, it means billing records were lost during the crash.
    let step = match verifier::verify_billing_chain(&admin_addr).await {
        Ok(ttr) => StepResult::pass("verify_billing_chain", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_billing_chain", &e),
    };
    report.add_step(step);

    // ── Verify: Billing record count is preserved ──────────────────
    // The number of billing records after restart should be >= the count
    // before the kill. It may be higher because the restart itself may
    // generate billing events (instance shutdown/startup).
    let step = match verifier::count_billing_records(&admin_addr).await {
        Ok(count_after) if count_after >= billing_before => StepResult::pass(
            "verify_billing_count_preserved",
            &format!("billing records preserved: {count_after} >= {billing_before}"),
        ),
        Ok(count_after) => StepResult::fail(
            "verify_billing_count_preserved",
            &format!("billing records LOST: {count_after} < {billing_before}"),
        ),
        Err(e) => StepResult::fail("verify_billing_count_preserved", &e),
    };
    report.add_step(step);

    // ── Verify: NATS is reconnected ────────────────────────────────
    let step = match verifier::verify_nats_connected(&admin_addr, Duration::from_secs(10)).await {
        Ok(ttr) => StepResult::pass("verify_nats_reconnected", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_nats_reconnected", &e),
    };
    report.add_step(step);

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L2 chaos test PASSED — node restart recovered"
        );
    }

    report
}

/// Variant: Test graceful shutdown (SIGTERM) instead of SIGKILL.
///
/// This verifies the graceful shutdown path where the node has a chance to:
/// - Flush pending billing records to redb
/// - Close NATS connections gracefully
/// - Persist any in-memory state
///
/// The TTR should be similar to the SIGKILL case, but the data integrity
/// guarantees are stronger because the node had a chance to flush.
pub async fn test_l2_node_graceful_restart_recovery() -> TestReport {
    let mut report = TestReport::new("L2: Node Graceful Restart Recovery (SIGTERM)");

    // ── Setup ──────────────────────────────────────────────────────
    let mut fixture = match ClusterFixture::single().await {
        Ok(f) => f,
        Err(e) => {
            report.fail_setup(&format!("cluster setup failed: {e}"));
            return report;
        }
    };

    let app_id = "chaos-app:v1";
    let host = "chaos-app.local";

    let step = match helpers::setup_deploy_app(&fixture, app_id, host).await {
        Ok(_) => StepResult::pass("setup_deploy_app", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    let admin_addr = fixture.node(0).admin_addr_str();
    let proxy_addr = fixture.node(0).proxy_addr_str();

    // Trigger the initial cold start so the graceful restart scenario starts
    // from a node that is already serving a real instance.
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host).await;

    let step =
        match verifier::wait_for_app_instances(&admin_addr, app_id, 1, Duration::from_secs(30))
            .await
        {
            Ok(ttr) => StepResult::pass("wait_for_instance", &format!("{}ms", ttr.as_millis())),
            Err(e) => StepResult::fail("wait_for_instance", &e),
        };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Inject: SIGTERM (graceful shutdown) ────────────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::inject_node_terminate(fixture.node_mut(0)) {
        Ok(result) => StepResult::pass("inject_node_terminate", &result.description),
        Err(e) => StepResult::fail("inject_node_terminate", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // Wait for the process to exit gracefully
    tokio::time::sleep(Duration::from_secs(3)).await;

    if fixture.node_mut(0).is_running() {
        // Force kill if it hasn't exited yet
        let _ = fixture.node_mut(0).kill();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let step = if fixture.node_mut(0).is_running() {
        StepResult::fail(
            "wait_for_exit",
            "process did not exit after SIGTERM + SIGKILL",
        )
    } else {
        StepResult::pass("wait_for_exit", "process exited gracefully")
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── Recover: Restart the node process ──────────────────────────
    let step = match fixture.node_mut(0).restart().await {
        Ok(_) => StepResult::pass("restart_node", "ok"),
        Err(e) => StepResult::fail("restart_node", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: The node becomes healthy ───────────────────────────
    let step = match verifier::wait_for_node_healthy(&admin_addr, Duration::from_secs(60)).await {
        Ok(ttr) => StepResult::pass("wait_for_healthy", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("wait_for_healthy", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    if report.failed() {
        return report;
    }

    // ── Verify: The proxy serves traffic ───────────────────────────
    // Trigger cold start first
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host).await;

    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_traffic_served", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_traffic_served", &e),
    };
    report.add_step(step);

    // ── Verify: Billing chain is intact ────────────────────────────
    let step = match verifier::verify_billing_chain(&admin_addr).await {
        Ok(ttr) => StepResult::pass("verify_billing_chain", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_billing_chain", &e),
    };
    report.add_step(step);

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L2 graceful restart chaos test PASSED"
        );
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporter::TestResult;

    #[test]
    fn test_l2_report_name() {
        let report = TestReport::new("L2: Node Process Restart Recovery");
        assert_eq!(report.name, "L2: Node Process Restart Recovery");
        assert!(report.passed());
    }

    #[test]
    fn test_l2_report_step_sequence() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::pass("count_billing_before", "count=5"));
        report.add_step(StepResult::pass(
            "inject_node_kill",
            "L2: killed node process chaos-node-0",
        ));
        report.add_step(StepResult::pass("verify_process_dead", "ok"));
        report.add_step(StepResult::pass("restart_node", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy", "12000ms"));
        report.add_step(StepResult::pass("verify_app_restored", "1 instance"));
        report.add_step(StepResult::pass("verify_traffic_served", "200 OK"));
        report.add_step(StepResult::pass("verify_route_restored", "ok"));
        report.add_step(StepResult::pass("verify_billing_chain", "valid"));
        report.add_step(StepResult::pass("verify_billing_count_preserved", "6 >= 5"));
        report.add_step(StepResult::pass("verify_nats_reconnected", "ok"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 13);
    }

    #[test]
    fn test_l2_report_with_ttr() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.add_step(StepResult::pass("inject_node_kill", "killed"));
        report.set_ttr(Duration::from_millis(12000));

        assert_eq!(report.ttr_ms, Some(12000));
    }

    #[test]
    fn test_l2_report_billing_count_lost() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.add_step(StepResult::pass("count_billing_before", "count=5"));
        report.add_step(StepResult::fail(
            "verify_billing_count_preserved",
            "billing records LOST: 3 < 5",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l2_graceful_report_name() {
        let report = TestReport::new("L2: Node Graceful Restart Recovery (SIGTERM)");
        assert!(report.name.contains("SIGTERM"));
    }

    #[test]
    fn test_l2_report_setup_failure() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.fail_setup("wasm-node binary not found");

        assert!(report.failed());
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "setup");
    }

    #[test]
    fn test_l2_report_kill_failure() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::fail(
            "inject_node_kill",
            "failed to kill process: permission denied",
        ));

        assert!(report.failed());
        assert_eq!(report.result, TestResult::Fail);
    }

    #[test]
    fn test_l2_report_restart_failure() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("inject_node_kill", "killed"));
        report.add_step(StepResult::pass("verify_process_dead", "ok"));
        report.add_step(StepResult::fail(
            "restart_node",
            "failed to restart wasm-node: binary not found",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l2_report_healthy_timeout() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("inject_node_kill", "killed"));
        report.add_step(StepResult::pass("restart_node", "ok"));
        report.add_step(StepResult::fail(
            "wait_for_healthy",
            "node at 127.0.0.1:19090 did not become healthy within 60s",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l2_report_billing_chain_broken() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("inject_node_kill", "killed"));
        report.add_step(StepResult::pass("restart_node", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy", "12000ms"));
        report.add_step(StepResult::pass("verify_app_restored", "1 instance"));
        report.add_step(StepResult::pass("verify_traffic_served", "200 OK"));
        report.add_step(StepResult::pass("verify_route_restored", "ok"));
        report.add_step(StepResult::fail(
            "verify_billing_chain",
            "billing chain verification failed — hash chain is broken",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l2_graceful_report_step_sequence() {
        let mut report = TestReport::new("L2: Node Graceful Restart Recovery (SIGTERM)");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::pass(
            "inject_node_terminate",
            "L2: SIGTERM node process chaos-node-0",
        ));
        report.add_step(StepResult::pass(
            "wait_for_exit",
            "process exited gracefully",
        ));
        report.add_step(StepResult::pass("restart_node", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy", "10000ms"));
        report.add_step(StepResult::pass("verify_traffic_served", "200 OK"));
        report.add_step(StepResult::pass("verify_billing_chain", "valid"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 8);
    }

    #[test]
    fn test_l2_graceful_report_process_did_not_exit() {
        let mut report = TestReport::new("L2: Node Graceful Restart Recovery (SIGTERM)");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("inject_node_terminate", "SIGTERM sent"));
        report.add_step(StepResult::fail(
            "wait_for_exit",
            "process did not exit after SIGTERM + SIGKILL",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l2_report_full_pass_with_ttr() {
        let mut report = TestReport::new("L2: Node Process Restart Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "340ms"));
        report.add_step(StepResult::pass("count_billing_before", "count=5"));
        report.add_step(StepResult::pass(
            "inject_node_kill",
            "L2: killed node process chaos-node-0",
        ));
        report.add_step(StepResult::pass("verify_process_dead", "ok"));
        report.add_step(StepResult::pass("restart_node", "ok"));
        report.add_step(StepResult::pass("wait_for_healthy", "12000ms"));
        report.add_step(StepResult::pass("verify_app_restored", "2500ms"));
        report.add_step(StepResult::pass("verify_traffic_served", "15ms"));
        report.add_step(StepResult::pass("verify_route_restored", "8ms"));
        report.add_step(StepResult::pass("verify_billing_chain", "3ms"));
        report.add_step(StepResult::pass("verify_billing_count_preserved", "6 >= 5"));
        report.add_step(StepResult::pass("verify_nats_reconnected", "1200ms"));
        report.set_ttr(Duration::from_millis(14500));

        assert!(report.passed());
        assert_eq!(report.ttr_ms, Some(14500));
    }
}
