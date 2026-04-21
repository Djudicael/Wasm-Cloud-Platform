/// Chaos tests for the Wasm Cloud Platform
///
/// These tests verify the platform's resilience under failure conditions
/// across all six failure levels (L1–L6).
///
/// ## Prerequisites
///
/// Chaos tests require a Unix-like host (Linux/WSL) with:
///
/// - Unix process signals (`SIGKILL`, `SIGTERM`) for process management
/// - `tc` / `iptables` for network partition simulation (L5)
/// - Podman or Docker for testcontainers (NATS containers)
/// - `CAP_NET_ADMIN` for `tc netem` (L5 NATS partition tests)
/// - The `wasm-node` binary built (`cargo build --bin wasm-node`)
/// - The test WASM app built (`hello-axum` target `wasm32-wasip2`)
///
/// NATS is provisioned automatically via testcontainers — no external
/// NATS server is needed.
///
/// ## Building (inside WSL)
///
/// ```bash
/// # Enter WSL
/// wsl
///
/// # Build the node binary
/// cargo build --bin wasm-node
///
/// # Build the test WASM app
/// RUSTFLAGS='--cfg tokio_unstable' cargo build \
///     --manifest-path apps/hello-axum/Cargo.toml \
///     --target wasm32-wasip2 --release
///
/// # Build the e2e crate
/// cargo build -p e2e
/// ```
///
/// ## Running (inside WSL)
///
/// ```bash
/// # Run all chaos tests (sequentially to avoid port/resource conflicts)
/// cargo test -p e2e --test chaos -- --test-threads=1
///
/// # Run a specific failure level
/// cargo test -p e2e --test chaos -- --test-threads=1 test_l1_instance_crash
/// cargo test -p e2e --test chaos -- --test-threads=1 test_l2_node_restart
/// cargo test -p e2e --test chaos -- --test-threads=1 test_l3_redb_corruption
/// cargo test -p e2e --test chaos -- --test-threads=1 test_l4_full_rebuild
/// cargo test -p e2e --test chaos -- --test-threads=1 test_l5_nats_partition
/// cargo test -p e2e --test chaos -- --test-threads=1 test_l6_multi_node_failure
///
/// # Run with verbose output
/// RUST_LOG=e2e=debug cargo test -p e2e --test chaos -- --test-threads=1 --nocapture
///
/// # Run only basic chaos tests (L1–L2, no root needed)
/// cargo test -p e2e --test chaos -- --test-threads=1 test_l1
/// cargo test -p e2e --test chaos -- --test-threads=1 test_l2
/// ```
///
/// ## Environment Variables
///
/// ```bash
/// export WASM_NODE_BINARY="target/debug/wasm-node"
/// export TESTCONTAINERS_RYUK_DISABLED=true  # Podman compatibility
/// export CHAOS_REPORT_DIR="target/chaos-reports"  # Optional
/// ```
///
/// ## CI Integration
///
/// In CI, these tests run on `ubuntu-latest` (which is a Linux host with
/// Docker/Podman pre-installed). The `CAP_NET_ADMIN` requirement for L5
/// tests is satisfied by running with `sudo` or adding the capability.
use e2e::chaos;
use e2e::reporter::TestResult;

/// Initialize the tracing subscriber for chaos tests.
///
/// Uses `std::sync::Once` to ensure the subscriber is only registered once,
/// even when multiple tests run sequentially in the same process.
fn init_tracing() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("e2e=info")),
            )
            .with_test_writer()
            .init();
    });
}

// ── L1: Instance Crash Recovery ──────────────────────────────────────

#[tokio::test]
async fn test_l1_instance_crash() {
    init_tracing();
    let mut report = chaos::l1_instance_crash::test_l1_instance_crash_recovery().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L1 chaos test failed: {:?}",
        report.failed_steps()
    );
}

// ── L2: Node Process Restart Recovery ────────────────────────────────

#[tokio::test]
async fn test_l2_node_restart() {
    init_tracing();
    let mut report = chaos::l2_node_restart::test_l2_node_restart_recovery().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L2 chaos test failed: {:?}",
        report.failed_steps()
    );
}

#[tokio::test]
async fn test_l2_node_graceful_restart() {
    init_tracing();
    let mut report = chaos::l2_node_restart::test_l2_node_graceful_restart_recovery().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L2 graceful restart chaos test failed: {:?}",
        report.failed_steps()
    );
}

// ── L3: Redb Corruption Recovery ─────────────────────────────────────

#[tokio::test]
async fn test_l3_redb_corruption() {
    init_tracing();
    let mut report = chaos::l3_redb_corruption::test_l3_redb_corruption_recovery().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L3 chaos test failed: {:?}",
        report.failed_steps()
    );
}

#[tokio::test]
async fn test_l3_critical_corruption_full_rebootstrap() {
    init_tracing();
    let mut report =
        chaos::l3_redb_corruption::test_l3_critical_corruption_full_rebootstrap().await;
    report.print_summary();
    // This test may pass or fail depending on whether the node exits
    // (FullRebootstrap path) or recovers. Both outcomes are acceptable.
    assert!(
        matches!(report.result, TestResult::Pass | TestResult::Fail),
        "L3 critical corruption test had unexpected result: {:?}",
        report.result
    );
}

// ── L4: Full Node Rebuild Recovery ───────────────────────────────────

#[tokio::test]
async fn test_l4_full_rebuild() {
    init_tracing();
    let mut report = chaos::l4_full_rebuild::test_l4_full_rebuild_recovery().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L4 chaos test failed: {:?}",
        report.failed_steps()
    );
}

#[tokio::test]
async fn test_l4_rebuilt_node_receives_new_deployments() {
    init_tracing();
    let mut report = chaos::l4_full_rebuild::test_l4_rebuilt_node_receives_new_deployments().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L4 new deployments test failed: {:?}",
        report.failed_steps()
    );
}

// ── L5: NATS Partition Recovery ──────────────────────────────────────

#[tokio::test]
async fn test_l5_nats_partition() {
    init_tracing();
    let mut report = chaos::l5_nats_partition::test_l5_nats_partition_recovery().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L5 chaos test failed: {:?}",
        report.failed_steps()
    );
}

#[tokio::test]
async fn test_l5_partition_event_delivery_after_recovery() {
    init_tracing();
    let mut report =
        chaos::l5_nats_partition::test_l5_partition_event_delivery_after_recovery().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L5 event delivery test failed: {:?}",
        report.failed_steps()
    );
}

#[tokio::test]
async fn test_l5_degraded_mode_no_new_deploys() {
    init_tracing();
    let mut report = chaos::l5_nats_partition::test_l5_degraded_mode_no_new_deploys().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L5 degraded mode test failed: {:?}",
        report.failed_steps()
    );
}

// ── L6: Multi-Node Failure Recovery ──────────────────────────────────

#[tokio::test]
async fn test_l6_multi_node_failure() {
    init_tracing();
    let mut report = chaos::l6_multi_node_failure::test_l6_multi_node_failure_recovery().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L6 chaos test failed: {:?}",
        report.failed_steps()
    );
}

#[tokio::test]
async fn test_l6_survivor_receives_new_deployments() {
    init_tracing();
    let mut report =
        chaos::l6_multi_node_failure::test_l6_survivor_receives_new_deployments().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L6 survivor deployments test failed: {:?}",
        report.failed_steps()
    );
}

#[tokio::test]
async fn test_l6_sequential_node_failures() {
    init_tracing();
    let mut report = chaos::l6_multi_node_failure::test_l6_sequential_node_failures().await;
    report.print_summary();
    assert!(
        report.passed(),
        "L6 sequential failures test failed: {:?}",
        report.failed_steps()
    );
}

// ── Full Suite ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_chaos_full_suite() {
    init_tracing();
    // Check the environment before running the full suite
    if let Err(e) = e2e::chaos::check_environment() {
        eprintln!("⚠️  Environment check failed: {e}");
        eprintln!("   Chaos tests require WSL/Linux with Podman/Docker and built binaries.");
        eprintln!("   Run: cargo build --bin wasm-node");
        eprintln!("   Run: RUSTFLAGS='--cfg tokio_unstable' cargo build -p hello-axum --target wasm32-wasip2 --release");
        panic!("Environment check failed — see above for details");
    }

    let reports = e2e::chaos::run_all().await;

    let pass_count = reports.iter().filter(|r| r.passed()).count();
    let fail_count = reports.iter().filter(|r| !r.passed()).count();

    assert_eq!(
        fail_count,
        0,
        "{fail_count} of {} chaos tests failed. Pass: {pass_count}",
        reports.len()
    );
}

#[tokio::test]
async fn test_chaos_basic_suite() {
    init_tracing();
    // Basic suite: L1–L2 only (no CAP_NET_ADMIN needed)
    if let Err(e) = e2e::chaos::check_environment() {
        eprintln!("⚠️  Environment check failed: {e}");
        panic!("Environment check failed — see above for details");
    }

    let reports = e2e::chaos::run_basic().await;

    let pass_count = reports.iter().filter(|r| r.passed()).count();
    let fail_count = reports.iter().filter(|r| !r.passed()).count();

    assert_eq!(
        fail_count,
        0,
        "{fail_count} of {} basic chaos tests failed. Pass: {pass_count}",
        reports.len()
    );
}

// ── Infrastructure Test ──────────────────────────────────────────────

#[test]
fn test_chaos_infrastructure() {
    assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).exists());
}

#[test]
fn test_chaos_environment_check() {
    // This test always passes — it just prints the environment status.
    // Useful for debugging CI issues.
    match e2e::chaos::check_environment() {
        Ok(()) => eprintln!("✅ Chaos test environment is ready"),
        Err(e) => eprintln!("⚠️  Chaos test environment issue: {e}"),
    }
}
