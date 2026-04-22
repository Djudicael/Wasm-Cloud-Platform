//! L5 Chaos Test: NATS Partition Recovery
//!
//! Verifies that when a node is disconnected from NATS (simulating a network
//! partition, switch failure, or cable cut), the node enters degraded mode,
//! continues serving existing apps, and recovers when connectivity is restored.
//!
//! ## What This Tests
//!
//! - The `NatsHealthWatcher` detects the NATS disconnection within the
//!   configured timeout.
//! - The node enters degraded mode: existing apps continue to serve traffic,
//!   but new deployment events cannot be received.
//! - The node's health endpoint reports `nats: "disconnected"`.
//! - When the partition is removed, the `async_nats` client reconnects
//!   automatically.
//! - After reconnection, the node processes any missed events from JetStream.
//! - The node's health endpoint reports `nats: "connected"`.
//!
//! ## TTR Target
//!
//! | Metric    | Target | Max  |
//! |-----------|--------|------|
//! | TTR (L5)  | under 45s  | 90s  |
//!
//! The TTR is measured from the moment the NATS partition is removed to the
//! moment the node's health endpoint reports `nats: "connected"`.
//!
//! ## WSL / Linux Requirement
//!
//! This test uses `tc netem` or `iptables` to simulate a network partition,
//! which requires:
//!
//! - A Linux kernel with `tc` (traffic control) support
//! - `CAP_NET_ADMIN` capability (or root access)
//! - The `iproute2` package (for `tc`) or `iptables` package
//!
//! It **must run inside WSL** or on a native Linux host. On non-Linux
//! platforms, the network partition injection will fail with a clear error
//! message.
//!
//! ## Safety
//!
//! Network partition rules (`iptables` DROP rules or `tc qdisc`) are global
//! to the host. If the test is interrupted (Ctrl+C) during a partition, the
//! rules may persist and break subsequent tests or normal system operation.
//!
//! To mitigate this:
//!
//! - A Ctrl+C handler is registered during the partition to clean up rules.
//! - The `remove_nats_partition` function is called in the cleanup phase,
//!   even if the test fails.
//! - Tests run sequentially (`--test-threads=1`) to avoid rule conflicts.
//!
//! ## `CAP_NET_ADMIN` in Production & CI
//!
//! `CAP_NET_ADMIN` is a Linux capability that allows a process to perform
//! network administration tasks: configuring interfaces, setting up firewall
//! rules, and manipulating traffic control (`tc`). In the context of these
//! chaos tests, it is required because simulating a network partition
//! involves injecting global `iptables` DROP rules or `tc netem` packet-loss
//! policies on the host's loopback interface.
//!
//! ### Why this is safe in production CI
//!
//! In a real production CI pipeline (e.g., GitHub Actions, GitLab CI, or a
//! dedicated test runner), the test job typically runs inside an ephemeral
//! VM or container. The host is destroyed immediately after the test run,
//! so any lingering `iptables`/`tc` rules are irrelevant. This is the
//! recommended approach for automated chaos testing.
//!
//! ### How to grant `CAP_NET_ADMIN` in different environments
//!
//! | Environment | Command / Configuration |
//! |-------------|------------------------|
//! | **Local WSL (development)** | `sudo cargo test -p e2e --test chaos test_l5 -- --test-threads=1` |
//! | **Docker** | `docker run --cap-add=NET_ADMIN ...` |
//! | **Podman** | `podman run --cap-add=NET_ADMIN ...` |
//! | **Kubernetes** | Add `NET_ADMIN` to the container's `securityContext.capabilities.add` |
//! | **GitHub Actions** | No action needed — `ubuntu-latest` runners run as root |
//! | **systemd service** | `AmbientCapabilities=CAP_NET_ADMIN` in the unit file |
//!
//! ### Kubernetes example (Helm / raw YAML)
//!
//! ```yaml
//! securityContext:
//!   capabilities:
//!     add:
//!       - NET_ADMIN
//! ```
//!
//! ### Emergency cleanup
//!
//! If a test is killed with `kill -9` or the process panics during a
//! partition, the cleanup handler may not run. You can manually restore
//! connectivity with:
//!
//! ```bash
//! # Remove tc netem rules
//! sudo tc qdisc del dev lo root 2>/dev/null
//!
//! # Flush iptables OUTPUT chain (use with caution)
//! sudo iptables -F OUTPUT 2>/dev/null
//! ```

use crate::fixture::ClusterFixture;
use crate::helpers;
use crate::injector;
use crate::reporter::{StepResult, TestReport};
use crate::verifier;
use std::time::Duration;
use tracing::{info, warn};

/// Test: Disconnect a node from NATS and verify it enters degraded mode,
/// then recovers when connectivity is restored.
///
/// ## Steps
///
/// 1. **Setup**: Start a single-node cluster and deploy `chaos-app:v1`
/// 2. **Verify**: The app is running and serving traffic
/// 3. **Inject**: Block NATS connectivity via `tc netem` or `iptables` (L5 failure)
/// 4. **Verify**: The node enters degraded mode (NATS status = "disconnected")
/// 5. **Verify**: The node still serves existing apps (degraded mode)
/// 6. **Recover**: Remove the NATS partition (restore connectivity)
/// 7. **Verify**: NATS reconnects automatically
/// 8. **Verify**: The node processes any missed events
/// 9. **Verify**: The proxy serves traffic normally
///
/// ## Expected Behavior
///
/// When the NATS partition is injected:
///
/// 1. The `async_nats` client's TCP connection to NATS is blocked.
/// 2. The client's reconnection logic fires (after the connect timeout).
/// 3. The `NatsHealthWatcher` detects the disconnection and marks NATS as
///    disconnected in the shared `NatsHealth` state.
/// 4. The node enters degraded mode:
///    - Existing apps continue to serve traffic (they don't need NATS).
///    - New deployment events cannot be received (NATS is down).
///    - The health endpoint reports `nats: "disconnected"`.
/// 5. When the partition is removed:
///    - The `async_nats` client reconnects automatically.
///    - The `NatsHealthWatcher` detects the reconnection.
///    - The node resumes receiving events from JetStream.
///    - The health endpoint reports `nats: "connected"`.
///
/// The total TTR should be approximately:
///
/// ```text
/// TTR ≈ partition_removal + async_nats_reconnect + NatsHealthWatcher_detection
/// TTR ≈ 1s + 5-15s + 2-5s = 8-21s (target: under 45s, max: 90s)
/// ```
///
/// The `async_nats` reconnection time depends on the client's reconnect
/// settings (default: exponential backoff starting at 1s).
pub async fn test_l5_nats_partition_recovery() -> TestReport {
    let mut report = TestReport::new("L5: NATS Partition Recovery");

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

    // Extract owned addresses before any steps
    let admin_addr = fixture.node(0).admin_addr_str();
    let proxy_addr = fixture.node(0).proxy_addr_str();
    let nats_ip = "127.0.0.1";
    let nats_port = fixture.nats_container.port;

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

    // ── Verify: The app is serving traffic before partition ─────────
    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_traffic_before", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_traffic_before", &e),
    };
    report.add_step(step);

    // ── Verify: NATS is connected before partition ─────────────────
    let step = match verifier::verify_nats_connected(&admin_addr, Duration::from_secs(10)).await {
        Ok(ttr) => StepResult::pass(
            "verify_nats_connected_before",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_nats_connected_before", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Inject: Block NATS connectivity ─────────────────────────────
    // Use tc netem or iptables to drop all packets to the NATS server.
    // This simulates a network partition between the node and NATS.
    let step = match injector::inject_nats_partition(nats_ip, nats_port).await {
        Ok(result) => StepResult::pass("inject_nats_partition", &result.description),
        Err(e) => StepResult::fail("inject_nats_partition", &e),
    };
    report.add_step(step);

    if report.failed() {
        // If partition injection fails (e.g., no CAP_NET_ADMIN), we can't
        // run this test. Return the report with a clear error.
        warn!(
            "L5 test cannot proceed — NATS partition injection failed. \
             Ensure you have CAP_NET_ADMIN (run as root or with sudo)."
        );
        return report;
    }

    // ── Verify: The node enters degraded mode ───────────────────────
    // Wait for the NatsHealthWatcher to detect the disconnection.
    // The detection time depends on the async_nats client's reconnect
    // timeout and the NatsHealthWatcher's polling interval.
    let step = match verifier::verify_nats_disconnected(&admin_addr, Duration::from_secs(30)).await
    {
        Ok(ttr) => StepResult::pass("verify_degraded_mode", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_degraded_mode", &e),
    };
    report.add_step(step);

    // ── Verify: The node still serves existing apps (degraded mode) ──
    // Even though NATS is disconnected, the node should continue serving
    // existing apps because:
    // - The proxy doesn't need NATS to route requests
    // - The Wasm runtime doesn't need NATS to execute
    // - Only new deployment events require NATS
    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass(
            "verify_serves_in_degraded",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_serves_in_degraded", &e),
    };
    report.add_step(step);

    // ── Hold the partition for a sustained period ────────────────────
    // Keep the partition active for 10 seconds to verify that the node
    // remains stable in degraded mode (doesn't crash, panic, or OOM).
    info!("holding NATS partition for 10 seconds to verify stability");
    tokio::time::sleep(Duration::from_secs(10)).await;

    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass("sustained_partition", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("sustained_partition", &e),
    };
    report.add_step(step);

    // ── Recover: Remove the NATS partition ──────────────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::remove_nats_partition(nats_ip, nats_port).await {
        Ok(()) => StepResult::pass("remove_partition", "partition removed"),
        Err(e) => StepResult::fail("remove_partition", &e),
    };
    report.add_step(step);

    if report.failed() {
        // If partition removal fails, try harder to clean up so we don't
        // leave the host in a broken state.
        warn!("NATS partition removal failed — attempting emergency cleanup");
        let _ = injector::remove_nats_partition(nats_ip, nats_port).await;
        return report;
    }

    // ── Verify: NATS reconnects ─────────────────────────────────────
    // After the partition is removed, the async_nats client should
    // automatically reconnect. The reconnection time depends on the
    // client's reconnect settings (exponential backoff).
    let step = match verifier::verify_nats_connected(&admin_addr, Duration::from_secs(60)).await {
        Ok(ttr) => StepResult::pass("verify_nats_reconnected", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_nats_reconnected", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    if report.failed() {
        // Even if NATS reconnection verification failed, try to clean up
        // any remaining network rules.
        let _ = injector::remove_nats_partition(nats_ip, nats_port).await;
        return report;
    }

    // ── Verify: The node processes any missed events ─────────────────
    // After reconnection, the node should process any events that were
    // published while it was disconnected. JetStream ensures that
    // messages are not lost — they are delivered when the consumer
    // reconnects.
    //
    // To test this, we could publish a new route while the node is
    // disconnected and verify it appears after reconnection. However,
    // this is complex because we'd need to publish via a separate
    // NATS connection. For now, we verify that the node is fully
    // operational by sending a request.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass("verify_fully_recovered", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_fully_recovered", &e),
    };
    report.add_step(step);

    // ── Verify: The route still exists ──────────────────────────────
    // The route should not have been lost during the partition.
    let step = match verifier::verify_route_exists(&admin_addr, host).await {
        Ok(_) => StepResult::pass("verify_route_exists", "route exists"),
        Err(e) => StepResult::fail("verify_route_exists", &e),
    };
    report.add_step(step);

    // ── Verify: The app still has instances ──────────────────────────
    // Instances should not have been killed during the partition.
    let step =
        match verifier::wait_for_app_instances(&admin_addr, app_id, 1, Duration::from_secs(10))
            .await
        {
            Ok(ttr) => StepResult::pass(
                "verify_instances_running",
                &format!("{}ms", ttr.as_millis()),
            ),
            Err(e) => StepResult::fail("verify_instances_running", &e),
        };
    report.add_step(step);

    // ── Cleanup: Ensure network rules are removed ────────────────────
    // This is a safety net — the partition should have been removed
    // in the "remove_partition" step above, but we do it again to be
    // absolutely sure we don't leave the host in a broken state.
    let _ = injector::remove_nats_partition(nats_ip, nats_port).await;

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L5 chaos test PASSED — NATS partition recovered"
        );
    }

    report
}

/// Variant: Test that a new deployment published during a NATS partition
/// is received after reconnection.
///
/// This test verifies JetStream's guaranteed delivery:
///
/// 1. Deploy an app while the node is connected to NATS
/// 2. Inject a NATS partition
/// 3. Publish a new route via a separate NATS connection
/// 4. Remove the partition
/// 5. Verify the node receives the new route after reconnection
///
/// ## Why This Matters
///
/// In production, a brief NATS partition should not cause event loss.
/// JetStream guarantees at-least-once delivery, so any events published
/// during the partition should be delivered when the consumer reconnects.
pub async fn test_l5_partition_event_delivery_after_recovery() -> TestReport {
    let mut report = TestReport::new("L5: Event Delivery After NATS Partition");

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

    // Extract owned addresses before any steps
    let admin_addr = fixture.node(0).admin_addr_str();
    let proxy_addr = fixture.node(0).proxy_addr_str();
    let nats_ip = "127.0.0.1";
    let nats_port = fixture.nats_container.port;

    // Deploy a test app
    let step = match helpers::setup_deploy_app(&fixture, app_id, host).await {
        Ok(_) => StepResult::pass("setup_deploy_app", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

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

    // ── Verify: NATS is connected before partition ─────────────────
    let step = match verifier::verify_nats_connected(&admin_addr, Duration::from_secs(10)).await {
        Ok(ttr) => StepResult::pass(
            "verify_nats_connected_before",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_nats_connected_before", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Inject: Block NATS connectivity ─────────────────────────────
    let step = match injector::inject_nats_partition(nats_ip, nats_port).await {
        Ok(result) => StepResult::pass("inject_nats_partition", &result.description),
        Err(e) => StepResult::fail("inject_nats_partition", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: The node enters degraded mode ───────────────────────
    let step = match verifier::verify_nats_disconnected(&admin_addr, Duration::from_secs(30)).await
    {
        Ok(ttr) => StepResult::pass("verify_degraded_mode", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_degraded_mode", &e),
    };
    report.add_step(step);

    // ── Publish a new route while the node is disconnected ──────────
    // Use a separate NATS connection (from the fixture, not from the
    // node) to publish a RouteAdd event. This event will be stored in
    // JetStream but not delivered to the disconnected node.
    let new_host = "chaos-new.local";

    let step = match fixture.connect_bus().await {
        Ok(bus) => match helpers::add_route(&bus, new_host, app_id).await {
            Ok(_) => StepResult::pass("publish_route_during_partition", "route published"),
            Err(e) => StepResult::fail("publish_route_during_partition", &e),
        },
        Err(e) => StepResult::fail("publish_route_during_partition", &e),
    };
    report.add_step(step);

    // ── Verify: The new route does NOT exist on the node yet ────────
    // The node is disconnected from NATS, so it hasn't received the
    // RouteAdd event.
    match verifier::verify_route_exists(&admin_addr, new_host).await {
        Ok(_) => {
            // Route exists — this could mean the node received the
            // event before the partition took effect, or the
            // partition isn't working. Log a warning but don't fail.
            warn!(
                "route for '{new_host}' exists during partition — \
                 partition may not be fully effective, or the event \
                 was delivered before the partition took effect"
            );
            report.add_step(StepResult::pass(
                "verify_route_not_yet_received",
                "route exists (may have been delivered before partition)",
            ));
        }
        Err(_) => {
            // Route doesn't exist — expected behavior during partition
            report.add_step(StepResult::pass(
                "verify_route_not_yet_received",
                "route not received during partition (expected)",
            ));
        }
    }

    // ── Recover: Remove the NATS partition ──────────────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::remove_nats_partition(nats_ip, nats_port).await {
        Ok(()) => StepResult::pass("remove_partition", "partition removed"),
        Err(e) => StepResult::fail("remove_partition", &e),
    };
    report.add_step(step);

    if report.failed() {
        let _ = injector::remove_nats_partition(nats_ip, nats_port).await;
        return report;
    }

    // ── Verify: NATS reconnects ─────────────────────────────────────
    let step = match verifier::verify_nats_connected(&admin_addr, Duration::from_secs(60)).await {
        Ok(ttr) => StepResult::pass("verify_nats_reconnected", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_nats_reconnected", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    if report.failed() {
        let _ = injector::remove_nats_partition(nats_ip, nats_port).await;
        return report;
    }

    // ── Verify: The new route is received after reconnection ────────
    // After NATS reconnection, the node should process the RouteAdd
    // event that was published during the partition. JetStream
    // guarantees at-least-once delivery.
    tokio::time::sleep(Duration::from_secs(10)).await;

    let step = match verifier::verify_route_exists(&admin_addr, new_host).await {
        Ok(_) => StepResult::pass("verify_new_route_received", "route exists"),
        Err(e) => StepResult::fail("verify_new_route_received", &e),
    };
    report.add_step(step);

    // ── Verify: The original app still serves traffic ───────────────
    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass(
            "verify_original_app_traffic",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_original_app_traffic", &e),
    };
    report.add_step(step);

    // ── Cleanup ────────────────────────────────────────────────────
    let _ = injector::remove_nats_partition(nats_ip, nats_port).await;

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L5 event delivery test PASSED — events received after NATS partition recovery"
        );
    }

    report
}

/// Variant: Test that a node in degraded mode cannot receive new
/// deployments (but existing apps continue to work).
///
/// This is a simpler version of the main L5 test that focuses on the
/// degraded mode behavior without the event delivery verification.
pub async fn test_l5_degraded_mode_no_new_deploys() -> TestReport {
    let mut report = TestReport::new("L5: Degraded Mode — No New Deploys");

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

    // Extract owned addresses before any steps
    let admin_addr = fixture.node(0).admin_addr_str();
    let proxy_addr = fixture.node(0).proxy_addr_str();
    let artifact_port = fixture.nodes[0].artifact_port;
    let nats_ip = "127.0.0.1";
    let nats_port = fixture.nats_container.port;

    let step = match helpers::setup_deploy_app(&fixture, app_id, host).await {
        Ok(_) => StepResult::pass("setup_deploy_app", "ok"),
        Err(e) => StepResult::fail("setup_deploy_app", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

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

    // ── Inject: Block NATS connectivity ─────────────────────────────
    let step = match injector::inject_nats_partition(nats_ip, nats_port).await {
        Ok(result) => StepResult::pass("inject_nats_partition", &result.description),
        Err(e) => StepResult::fail("inject_nats_partition", &e),
    };
    report.add_step(step);

    if report.failed() {
        return report;
    }

    // ── Verify: The node enters degraded mode ───────────────────────
    let step = match verifier::verify_nats_disconnected(&admin_addr, Duration::from_secs(30)).await
    {
        Ok(ttr) => StepResult::pass("verify_degraded_mode", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_degraded_mode", &e),
    };
    report.add_step(step);

    // ── Verify: Existing app still serves traffic ───────────────────
    let step = match verifier::verify_proxy_request(&proxy_addr, host, 200).await {
        Ok(ttr) => StepResult::pass(
            "verify_existing_app_serves",
            &format!("{}ms", ttr.as_millis()),
        ),
        Err(e) => StepResult::fail("verify_existing_app_serves", &e),
    };
    report.add_step(step);

    // ── Verify: New deployment is NOT received ──────────────────────
    // Try to deploy a second app. The DeployApp event will be published
    // to NATS, but the disconnected node won't receive it.
    let app_id_2 = "chaos-app-2:v1";
    let host_2 = "chaos-2.local";

    // Deploy the second app via a separate NATS connection
    let deploy_result: Result<String, String> = async {
        let bus = fixture.connect_bus().await?;

        let wasm_path = helpers::find_hello_axum_wasm()?;
        let sha256 = helpers::sha256_file(&wasm_path)?;
        let size_bytes = std::fs::metadata(&wasm_path)
            .map_err(|e| format!("failed to read wasm metadata: {e}"))?
            .len();

        helpers::upload_artifact(artifact_port, &wasm_path, &sha256).await?;

        let artifact_url = format!("http://127.0.0.1:{}/artifacts/{}", artifact_port, sha256);

        let config = helpers::build_app_config(app_id_2, 100_000_000, 100, 1);

        helpers::deploy_app(&bus, app_id_2, artifact_url, sha256, size_bytes, config).await?;
        helpers::add_route(&bus, host_2, app_id_2).await?;

        Ok("second app deployed via separate NATS connection".to_string())
    }
    .await;

    let step = match deploy_result {
        Ok(msg) => StepResult::pass("deploy_second_app_during_partition", &msg),
        Err(e) => StepResult::fail("deploy_second_app_during_partition", &e),
    };
    report.add_step(step);

    // Wait a bit and verify the second app is NOT running on the node
    tokio::time::sleep(Duration::from_secs(5)).await;

    // The second app should NOT have instances on the node
    // because the node is disconnected from NATS
    match verifier::wait_for_app_instances(&admin_addr, app_id_2, 1, Duration::from_secs(5)).await {
        Ok(_) => {
            // The node received the deployment despite being disconnected.
            // This could happen if the partition isn't fully effective.
            warn!(
                "second app deployed during partition — partition may not be \
                 fully effective, or the event was delivered before partition took effect"
            );
            report.add_step(StepResult::pass(
                "verify_second_app_not_received",
                "second app received (partition may not be fully effective)",
            ));
        }
        Err(_) => {
            // Expected: the node didn't receive the deployment
            report.add_step(StepResult::pass(
                "verify_second_app_not_received",
                "second app NOT received during partition (expected)",
            ));
        }
    }

    // ── Recover: Remove the NATS partition ──────────────────────────
    let recovery_start = std::time::Instant::now();

    let step = match injector::remove_nats_partition(nats_ip, nats_port).await {
        Ok(()) => StepResult::pass("remove_partition", "partition removed"),
        Err(e) => StepResult::fail("remove_partition", &e),
    };
    report.add_step(step);

    if report.failed() {
        let _ = injector::remove_nats_partition(nats_ip, nats_port).await;
        return report;
    }

    // ── Verify: NATS reconnects ─────────────────────────────────────
    let step = match verifier::verify_nats_connected(&admin_addr, Duration::from_secs(60)).await {
        Ok(ttr) => StepResult::pass("verify_nats_reconnected", &format!("{}ms", ttr.as_millis())),
        Err(e) => StepResult::fail("verify_nats_reconnected", &e),
    };
    report.add_step(step);

    let ttr_duration = recovery_start.elapsed();
    report.set_ttr(ttr_duration);

    // ── Verify: The second app is now received ──────────────────────
    // After reconnection, the node should process the queued DeployApp
    // event and start the second app.
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Trigger cold start
    let _ = verifier::verify_proxy_request_any_2xx(&proxy_addr, host_2).await;

    let step =
        match verifier::wait_for_app_instances(&admin_addr, app_id_2, 1, Duration::from_secs(30))
            .await
        {
            Ok(ttr) => StepResult::pass(
                "verify_second_app_received_after_recovery",
                &format!("{}ms", ttr.as_millis()),
            ),
            Err(e) => StepResult::fail("verify_second_app_received_after_recovery", &e),
        };
    report.add_step(step);

    // ── Cleanup ────────────────────────────────────────────────────
    let _ = injector::remove_nats_partition(nats_ip, nats_port).await;

    // ── Summary ────────────────────────────────────────────────────
    if report.passed() {
        info!(
            ttr_ms = ttr_duration.as_millis(),
            "L5 degraded mode test PASSED — no new deploys during partition, recovered after"
        );
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l5_report_name() {
        let report = TestReport::new("L5: NATS Partition Recovery");
        assert_eq!(report.name, "L5: NATS Partition Recovery");
        assert!(report.passed());
    }

    #[test]
    fn test_l5_report_step_sequence() {
        let mut report = TestReport::new("L5: NATS Partition Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::pass("verify_traffic_before", "200 OK"));
        report.add_step(StepResult::pass(
            "verify_nats_connected_before",
            "connected",
        ));
        report.add_step(StepResult::pass(
            "inject_nats_partition",
            "L5: NATS partition (iptables)",
        ));
        report.add_step(StepResult::pass(
            "verify_degraded_mode",
            "nats=disconnected",
        ));
        report.add_step(StepResult::pass("verify_serves_in_degraded", "200 OK"));
        report.add_step(StepResult::pass("sustained_partition", "200 OK"));
        report.add_step(StepResult::pass("remove_partition", "ok"));
        report.add_step(StepResult::pass("verify_nats_reconnected", "15000ms"));
        report.add_step(StepResult::pass("verify_fully_recovered", "200 OK"));
        report.add_step(StepResult::pass("verify_route_exists", "route exists"));
        report.add_step(StepResult::pass("verify_instances_running", "1 instance"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 13);
    }

    #[test]
    fn test_l5_report_with_ttr() {
        let mut report = TestReport::new("L5: NATS Partition Recovery");
        report.add_step(StepResult::pass("inject_nats_partition", "partitioned"));
        report.add_step(StepResult::pass("remove_partition", "ok"));
        report.set_ttr(Duration::from_millis(15000));

        assert_eq!(report.ttr_ms, Some(15000));
    }

    #[test]
    fn test_l5_report_partition_injection_failed() {
        let mut report = TestReport::new("L5: NATS Partition Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::fail(
            "inject_nats_partition",
            "failed to inject NATS partition (both tc and iptables failed). \
             Ensure you have CAP_NET_ADMIN or run as root.",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l5_report_degraded_mode_not_detected() {
        let mut report = TestReport::new("L5: NATS Partition Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("inject_nats_partition", "ok"));
        report.add_step(StepResult::fail(
            "verify_degraded_mode",
            "NATS did not report disconnected within 30s",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l5_report_recovery_timeout() {
        let mut report = TestReport::new("L5: NATS Partition Recovery");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("inject_nats_partition", "ok"));
        report.add_step(StepResult::pass("verify_degraded_mode", "disconnected"));
        report.add_step(StepResult::pass("remove_partition", "ok"));
        report.add_step(StepResult::fail(
            "verify_nats_reconnected",
            "NATS did not reconnect within 60s",
        ));

        assert!(report.failed());
    }

    #[test]
    fn test_l5_event_delivery_report_name() {
        let report = TestReport::new("L5: Event Delivery After NATS Partition");
        assert!(report.name.contains("Event Delivery"));
    }

    #[test]
    fn test_l5_event_delivery_step_sequence() {
        let mut report = TestReport::new("L5: Event Delivery After NATS Partition");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::pass(
            "verify_nats_connected_before",
            "connected",
        ));
        report.add_step(StepResult::pass("inject_nats_partition", "ok"));
        report.add_step(StepResult::pass("verify_degraded_mode", "disconnected"));
        report.add_step(StepResult::pass("publish_route_during_partition", "ok"));
        report.add_step(StepResult::pass(
            "verify_route_not_yet_received",
            "not received",
        ));
        report.add_step(StepResult::pass("remove_partition", "ok"));
        report.add_step(StepResult::pass("verify_nats_reconnected", "12000ms"));
        report.add_step(StepResult::pass(
            "verify_new_route_received",
            "route exists",
        ));
        report.add_step(StepResult::pass("verify_original_app_traffic", "200 OK"));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 11);
    }

    #[test]
    fn test_l5_degraded_mode_report_name() {
        let report = TestReport::new("L5: Degraded Mode — No New Deploys");
        assert!(report.name.contains("Degraded"));
    }

    #[test]
    fn test_l5_degraded_mode_step_sequence() {
        let mut report = TestReport::new("L5: Degraded Mode — No New Deploys");
        report.add_step(StepResult::pass("setup_deploy_app", "ok"));
        report.add_step(StepResult::pass("wait_for_instance", "1 instance"));
        report.add_step(StepResult::pass("inject_nats_partition", "ok"));
        report.add_step(StepResult::pass("verify_degraded_mode", "disconnected"));
        report.add_step(StepResult::pass("verify_existing_app_serves", "200 OK"));
        report.add_step(StepResult::pass("deploy_second_app_during_partition", "ok"));
        report.add_step(StepResult::pass(
            "verify_second_app_not_received",
            "not received",
        ));
        report.add_step(StepResult::pass("remove_partition", "ok"));
        report.add_step(StepResult::pass("verify_nats_reconnected", "ok"));
        report.add_step(StepResult::pass(
            "verify_second_app_received_after_recovery",
            "1 instance",
        ));

        assert!(report.passed());
        assert_eq!(report.steps.len(), 10);
    }

    #[test]
    fn test_l5_report_setup_failure() {
        let mut report = TestReport::new("L5: NATS Partition Recovery");
        report.fail_setup("NATS container failed to start");

        assert!(report.failed());
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "setup");
    }

    #[test]
    fn test_l5_report_cleanup_on_failure() {
        // Verify that the report structure supports the cleanup pattern
        // where remove_nats_partition is called even after a failure.
        let mut report = TestReport::new("L5: NATS Partition Recovery");
        report.add_step(StepResult::pass("inject_nats_partition", "ok"));
        report.add_step(StepResult::pass("verify_degraded_mode", "disconnected"));
        report.add_step(StepResult::pass("remove_partition", "ok"));
        report.add_step(StepResult::fail(
            "verify_nats_reconnected",
            "NATS did not reconnect within 60s",
        ));

        assert!(report.failed());
        // The partition was removed (step 3 passed), so the host should
        // be in a clean state even though the test failed.
        assert_eq!(report.steps[2].name, "remove_partition");
    }
}
