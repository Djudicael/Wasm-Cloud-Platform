//! L1 Chaos Test: Instance Crash Recovery
//!
//! Verifies that when a single Wasm instance is killed (simulating an OOM or
//! trap), the Supervisor's health loop detects the dead instance, removes it
//! from the upstream table, and a new instance is spawned on the next request
//! (cold start).
//!
//! ## What This Tests
//!
//! - The Supervisor's health loop detects dead instances within the
//!   configured `check_interval_secs` (default: 2s).
//! - The dead instance is removed from the upstream table so the proxy
//!   stops routing traffic to it.
//! - A subsequent request triggers a cold start, spawning a new instance.
//! - The new instance is registered in the upstream table and serves traffic.
//!
//! ## TTR Target
//!
//! | Metric    | Target | Max  |
//! |-----------|--------|------|
//! | TTR (L1)  | < 5s   | 10s  |
//!
//! The TTR is measured from the moment the instance is killed to the moment
//! the proxy successfully serves a request with a new instance.
//!
//! ## WSL Requirement
//!
//! This test uses the admin API to kill instances (not OS signals), so it
//! works on any platform. However, the full chaos test suite should run
//! inside WSL for consistency with other scenarios.

use crate::fixture::ClusterFixture;
use crate::helpers;
use crate::injector;
use crate::reporter::{StepResult, TestReport};
use crate::verifier;
use std::time::Duration;
use tracing::info;

/// Test: Kill a Wasm instance and verify the Supervisor detects it
/// and removes it from the upstream table within the health loop interval.
///
/// ## Steps
///
/// 1. **Setup**: Start a single-node cluster and deploy `chaos-app:v1`
/// 2. **Verify**: The app has at least 1 running instance
/// 3. **Inject**: Kill the instance via the admin API (L1 failure)
/// 4. **Verify**: The instance is removed from the upstream table
/// 5. **Verify**: A new instance is spawned on the next request (cold start)
/// 6. **Verify**: The proxy serves traffic successfully
///
/// ## Expected Behavior
///
/// The Supervisor's health loop runs every `check_interval_secs` (2s by
/// default). When it detects the dead instance, it:
///
/// 1. Removes the instance from the upstream table
/// 2. Publishes an `InstanceDead` event to NATS
/// 3. On the next incoming request, the proxy triggers a cold start
/// 4. The new instance is registered in the upstream table
///
/// The total TTR should be approximately:
///
/// ```text
/// TTR ≈ health_check_interval + cold_start_time
/// TTR ≈ 2s + 1-3s = 3-5s (target: < 5s, max: 10s)
/// ```
pub async fn test_l1_instance_crash_recovery() -> TestReport {
    let mut report = TestReport::new("L1: Instance Crash Recovery");

    // ── Setup ──────────────────────────────────────────────────────
    let fixture = match ClusterFixture::single().await {
        Ok(f) => f,
        Err(e) => {
            report.fail_setup(&format!("cluster setup failed: {e}"));
            return report;
        }
    };

    let app_id = "chaos-app:v1";
    let host = "chaos-app.local";
    let admin_addr = fixture.node(0).admin_addr_str();
    let proxy_addr = fixture.node(0).proxy_addr_str();

    // Deploy a test app
    let step = match helpers::setup_deploy_app(&fixture, app_id, host).await {
        Ok(_) => StepResult::pass("setup_deploy_app", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

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

    // ── Inject: Kill the instance ──────────────────────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::inject_instance_crash(&admin_addr, app_id).await {
        Ok(result) => StepResult::pass("inject_instance_crash", &result.description),
        Err(e) => StepResult::fail("inject_instance_crash", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: The instance is removed from the upstream table ─────
    // Wait for the health loop to detect the dead instance.
    // The health loop interval is 2s, so we wait a bit longer.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let step =
        match verifier::wait_for_app_zero_instances(&admin_addr, app_id, Duration::from_secs(10))
            .await
        {
            Ok(ttr) => {
                StepResult::pass("verify_instance_removed", &format!("{}ms", ttr.as_millis()))
            }
            Err(e) => StepResult::fail("verify_instance_removed", &e),
        };
    report.add_step(step);

    // ── Verify: A new instance is spawned (cold start on next request) ──
    // Send a request to trigger cold start
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host).await;

    let step =
        match verifier::wait_for_app_instances(&admin_addr, app_id, 1, Duration::from_secs(15))
            .await
        {
            Ok(ttr) => StepResult::pass(
                "verify_instance_respawned",
                &format!("{}ms", ttr.as_millis()),
            ),
            Err(e) => StepResult::fail("verify_instance_respawned", &e),
        };
    report.add_step(step);

    let ttr = recovery_start.elapsed();
    report.set_ttr(ttr);

    // ── Verify: The proxy serves traffic successfully ───────────────
    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_traffic_served", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_traffic_served", &e),
    };
    report.add_step(step);

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr.as_millis(),
            "L1 chaos test PASSED — instance crash recovered"
        );
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporter::TestResult;

    #[test]
    fn test_l1_report_name() {
        let report = TestReport::new("L1: Instance Crash Recovery");
        assert_eq!(report.name, "L1: Instance Crash Recovery");
        assert!(report.passed());
    }

    #[test]
    fn test_l1_report_step_naming() {
        let mut report = TestReport::new("L1: Instance Crash Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::pass(
            "inject_instance_crash",
            "L1: killed instance",
        ));
        report.add_step(StepResult::pass("verify_instance_removed", "0 instances"));
        report.add_step(StepResult::pass("verify_instance_respawned", "1 instance"));
        report.add_step(StepResult::pass("verify_traffic_served", "200 OK"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 6);
    }

    #[test]
    fn test_l1_report_with_ttr() {
        let mut report = TestReport::new("L1: Instance Crash Recovery");
        report.add_step(StepResult::pass("inject_instance_crash", "killed"));
        report.set_ttr(Duration::from_millis(3500));

        assert_eq!(report.ttr_ms, Some(3500));
    }

    #[test]
    fn test_l1_report_setup_failure() {
        let mut report = TestReport::new("L1: Instance Crash Recovery");
        report.fail_setup("NATS container failed to start");

        assert!(report.failed());
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "setup");
    }

    #[test]
    fn test_l1_report_inject_failure() {
        let mut report = TestReport::new("L1: Instance Crash Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::fail(
            "inject_instance_crash",
            "no running instances for chaos-app:v1 — cannot inject L1 crash",
        ));

        assert!(report.failed());
        assert_eq!(report.result, TestResult::Fail);
    }

    #[test]
    fn test_l1_report_verify_failure() {
        let mut report = TestReport::new("L1: Instance Crash Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::pass(
            "inject_instance_crash",
            "L1: killed instance",
        ));
        report.add_step(StepResult::fail(
            "verify_instance_removed",
            "app chaos-app:v1 still has instances after 10s",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l1_report_cold_start_timeout() {
        let mut report = TestReport::new("L1: Instance Crash Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("inject_instance_crash", "killed"));
        report.add_step(StepResult::pass("verify_instance_removed", "0 instances"));
        report.add_step(StepResult::fail(
            "verify_instance_respawned",
            "app chaos-app:v1 did not reach 1 instances within 15s",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l1_report_full_pass_with_ttr() {
        let mut report = TestReport::new("L1: Instance Crash Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "340ms"));
        report.add_step(StepResult::pass(
            "inject_instance_crash",
            "L1: killed instance of chaos-app:v1 (was 1 running)",
        ));
        report.add_step(StepResult::pass("verify_instance_removed", "2100ms"));
        report.add_step(StepResult::pass("verify_instance_respawned", "5200ms"));
        report.add_step(StepResult::pass("verify_traffic_served", "12ms"));
        report.set_ttr(Duration::from_millis(5200));

        assert!(report.passed());
        assert_eq!(report.ttr_ms, Some(5200));
    }
}
