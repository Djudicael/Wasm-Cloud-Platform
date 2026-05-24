//! Chaos test scenarios for the Wasm Cloud Platform.
//!
//! This module contains pre-built chaos test scenarios for each failure
//! level (L1â€“L6) defined in Step 27 (Disaster Recovery). Each scenario:
//!
//! 1. Sets up a cluster fixture (NATS + N wasm-node instances)
//! 2. Deploys a test application
//! 3. Injects a specific failure
//! 4. Verifies that the system recovers correctly
//! 5. Measures Time To Recovery (TTR)
//! 6. Produces a structured [`TestReport`](crate::reporter::TestReport)
//!
//! ## Running Chaos Tests
//!
//! Chaos tests **must run inside WSL** (Windows Subsystem for Linux) or on
//! a native Linux host because they require:
//!
//! - Unix process signals (`SIGKILL`, `SIGTERM`) for process management
//! - `tc` / `iptables` for network partition simulation (L5)
//! - Podman or Docker for host-managed NATS containers
//! - `CAP_NET_ADMIN` for scoped `tc` / `iptables` L5 NATS partition tests
//!
//! ```bash
//! # Build inside WSL
//! wsl cargo build -p e2e
//!
//! # Run all chaos tests inside WSL (sequential, requires NATS)
//! wsl cargo test -p e2e -- --ignored --test-threads=1 chaos
//!
//! # Run a specific failure level
//! wsl cargo test -p e2e -- --ignored --test-threads=1 test_l1_instance_crash
//! wsl cargo test -p e2e -- --ignored --test-threads=1 test_l4_full_rebuild
//! ```
//!
//! ## Failure Levels
//!
//! | Level | Module                    | Failure Type              | Target TTR | Max TTR |
//! |-------|---------------------------|---------------------------|------------|---------|
//! | L1    | `l1_instance_crash`      | Instance crash (OOM/trap) | < 5s       | 10s     |
//! | L2    | `l2_node_restart`        | Node process restart      | < 30s      | 60s     |
//! | L3    | `l3_redb_corruption`     | Redb partial corruption    | < 10s      | 30s     |
//! | L4    | `l4_full_rebuild`        | Full node rebuild         | < 120s     | 300s    |
//! | L5    | `l5_nats_partition`      | NATS partition (30s)       | < 45s      | 90s     |
//! | L6    | `l6_multi_node_failure`  | Multi-node failure         | < 300s     | 600s    |

pub mod l1_instance_crash;
pub mod l2_node_restart;
pub mod l3_redb_corruption;
pub mod l4_full_rebuild;
pub mod l5_nats_partition;
pub mod l6_multi_node_failure;

use crate::reporter::TestReport;

/// Run all chaos test scenarios sequentially and return their reports.
///
/// Each scenario creates its own [`ClusterFixture`](crate::fixture::ClusterFixture)
/// and cleans up on completion. Tests run sequentially because:
///
/// - They share the host's network namespace (port conflicts)
/// - Network partition tests (L5) modify global `iptables`/`tc` rules
/// - Memory pressure tests (extra) affect the entire host
///
/// After all scenarios complete, reports are saved to `target/chaos-reports/`
/// and a summary is printed to stdout.
pub async fn run_all() -> Vec<TestReport> {
    let mut reports = Vec::new();

    tracing::info!("â•â•â• Starting chaos test suite â•â•â•");

    // L1: Instance crash
    tracing::info!("â”€â”€ L1: Instance Crash Recovery â”€â”€");
    reports.push(l1_instance_crash::test_l1_instance_crash_recovery().await);

    // L2: Node restart
    tracing::info!("â”€â”€ L2: Node Process Restart Recovery â”€â”€");
    reports.push(l2_node_restart::test_l2_node_restart_recovery().await);

    // L3: Redb corruption
    tracing::info!("â”€â”€ L3: Redb Corruption Recovery â”€â”€");
    reports.push(l3_redb_corruption::test_l3_redb_corruption_recovery().await);

    // L4: Full rebuild
    tracing::info!("â”€â”€ L4: Full Node Rebuild Recovery â”€â”€");
    reports.push(l4_full_rebuild::test_l4_full_rebuild_recovery().await);

    // L5: NATS partition
    tracing::info!("â”€â”€ L5: NATS Partition Recovery â”€â”€");
    reports.push(l5_nats_partition::test_l5_nats_partition_recovery().await);

    // L6: Multi-node failure
    tracing::info!("â”€â”€ L6: Multi-Node Failure Recovery â”€â”€");
    reports.push(l6_multi_node_failure::test_l6_multi_node_failure_recovery().await);

    // Save all reports to disk
    for report in &mut reports {
        if let Err(e) = report.save_to_file() {
            tracing::warn!(error = %e, "failed to save chaos report");
        }
    }

    // Print combined summary
    crate::reporter::print_summary(&reports);

    tracing::info!("â•â•â• Chaos test suite complete â•â•â•");

    reports
}

/// Run only the "basic" chaos tests (L1â€“L2) that don't require root
/// or `CAP_NET_ADMIN`.
///
/// These tests only need:
/// - A running container runtime (Podman/Docker)
/// - The `wasm-node` binary
/// - A built test WASM app
///
/// No network-level manipulation is required.
pub async fn run_basic() -> Vec<TestReport> {
    let mut reports = Vec::new();

    tracing::info!("â•â•â• Starting basic chaos tests (L1â€“L2) â•â•â•");

    reports.push(l1_instance_crash::test_l1_instance_crash_recovery().await);
    reports.push(l2_node_restart::test_l2_node_restart_recovery().await);

    for report in &mut reports {
        if let Err(e) = report.save_to_file() {
            tracing::warn!(error = %e, "failed to save chaos report");
        }
    }

    crate::reporter::print_summary(&reports);

    tracing::info!("â•â•â• Basic chaos tests complete â•â•â•");

    reports
}

/// Check whether the current environment supports full chaos testing.
///
/// Returns `Ok(())` if all requirements are met, or an error describing
/// what's missing. Use this before calling [`run_all`] to provide a
/// clear error message instead of a cryptic test failure.
#[cfg(not(unix))]
pub fn check_environment() -> Result<(), String> {
    Err("chaos tests require a Unix-like system (Linux/WSL). \
         Process signals (SIGKILL, SIGTERM) are not available on this platform."
        .to_string())
}

#[cfg(unix)]
pub fn check_environment() -> Result<(), String> {
    // Check that the wasm-node binary exists
    let binary_path = crate::fixture::find_node_binary();
    if !std::path::Path::new(&binary_path).exists() {
        return Err(format!(
            "wasm-node binary not found at '{binary_path}' or 'target/release/wasm-node'. \
             Build it with: cargo build --bin wasm-node"
        ));
    }

    if crate::fixture::detect_host_container_runtime().is_none() {
        return Err("no supported container runtime found. Install Podman or Docker for the E2E NATS harness.".to_string());
    }

    // Check that tc is available (needed for L5 NATS partition)
    let tc_available = std::process::Command::new("tc")
        .arg("help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !tc_available {
        tracing::warn!(
            "tc (traffic control) not found - L5 NATS partition tests will use iptables fallback"
        );
    }

    // Check that iptables is available (fallback for L5)
    let iptables_available = std::process::Command::new("iptables")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !tc_available && !iptables_available {
        tracing::warn!(
            "neither tc nor iptables found - L5 NATS partition tests will fail. \
             Install iproute2 and/or iptables, or run with CAP_NET_ADMIN."
        );
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_environment_runs() {
        // Just verify it doesn't panic â€” the result depends on the environment
        let _ = check_environment();
    }
}
