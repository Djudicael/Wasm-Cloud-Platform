//! Fault injection primitives for chaos testing.
//!
//! Each function injects a specific type of failure and returns an
//! [`InjectionResult`] with a timestamp marking when the failure was
//! injected. This timestamp is used to compute Time To Recovery (TTR).
//!
//! ## Failure Levels
//!
//! | Level | Function                    | What It Does                        |
//! |-------|-----------------------------|--------------------------------------|
//! | L1    | `inject_instance_crash`     | Kills a specific Wasm instance       |
//! | L2    | `inject_node_kill`          | SIGKILL the entire wasm-node process |
//! | L3    | `inject_redb_corruption`    | Overwrites a redb data page          |
//! | L5    | `inject_nats_partition`     | Blocks NATS via tc/iptables          |
//! | Extra | `inject_disk_latency`       | Simulates slow I/O                   |
//! | Extra | `inject_memory_pressure`    | Allocates large memory               |
//!
//! ## WSL / Linux Requirement
//!
//! Several injection methods require Unix-specific APIs:
//!
//! - **L2** (`inject_node_kill`): Uses `SIGKILL` (no Windows equivalent).
//! - **L3** (`inject_redb_corruption`): Direct file I/O works everywhere, but
//!   the node's integrity check only runs on startup (Unix process model).
//! - **L5** (`inject_nats_partition`): Uses `tc netem` or `iptables`, which
//!   require `CAP_NET_ADMIN` and a Linux kernel.
//!
//! Run these inside WSL or on a native Linux host.

use std::io::{Seek, SeekFrom, Write};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Result of injecting a failure.
///
/// The `injected_at` timestamp is the basis for computing TTR:
///
/// ```text
/// TTR = Time(first healthy response after recovery) - injected_at
/// ```
#[derive(Debug)]
pub struct InjectionResult {
    /// When the failure was injected.
    pub injected_at: Instant,
    /// Human-readable description of the injected failure.
    pub description: String,
}

// ── L1: Instance Crash ───────────────────────────────────────────────

/// Inject an L1 failure: kill a specific Wasm instance.
///
/// Simulates an OOM kill or trap that terminates one instance of an app.
/// Uses the admin API to request an instance kill, which triggers the
/// Supervisor's health loop to detect the dead instance and remove it
/// from the upstream table.
///
/// The admin API endpoint `POST /admin/instances/{app_id}/kill` is used
/// rather than finding the PID directly, because Wasm instances run as
/// in-process Tokio tasks (not separate OS processes).
pub async fn inject_instance_crash(
    admin_addr: &str,
    app_id: &str,
) -> Result<InjectionResult, String> {
    let start = Instant::now();
    info!(app = app_id, "injecting L1 failure: instance crash");

    let client = reqwest::Client::new();

    // First, query the instances to confirm the app is running
    let list_url = format!("http://{admin_addr}/admin/instances/{app_id}");
    let resp = client
        .get(&list_url)
        .send()
        .await
        .map_err(|e| format!("failed to query instances: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "failed to list instances for {app_id}: status {}",
            resp.status()
        ));
    }

    let instances: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse instances response: {e}"))?;

    let instance_count = instances
        .get("instances")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    if instance_count == 0 {
        return Err(format!(
            "no running instances for {app_id} — cannot inject L1 crash"
        ));
    }

    // Kill the instance via the admin API
    let kill_url = format!("http://{admin_addr}/admin/instances/{app_id}/kill");
    let kill_resp = client
        .post(&kill_url)
        .send()
        .await
        .map_err(|e| format!("failed to kill instance: {e}"))?;

    if !kill_resp.status().is_success() {
        return Err(format!(
            "instance kill returned status {}: {}",
            kill_resp.status(),
            kill_resp.text().await.unwrap_or_default()
        ));
    }

    info!(
        app = app_id,
        instances = instance_count,
        "L1 instance crash injected"
    );

    Ok(InjectionResult {
        injected_at: start,
        description: format!("L1: killed instance of {app_id} (was {instance_count} running)"),
    })
}

// ── L2: Node Process Kill ────────────────────────────────────────────

/// Inject an L2 failure: kill the entire wasm-node process.
///
/// Sends `SIGKILL` to the node process, simulating an OOM kill, hardware
/// fault, or operator error. The process has no chance to run cleanup code,
/// which is the most realistic crash simulation.
///
/// After calling this, the caller should:
/// 1. Verify the process is dead (`node.is_running()`)
/// 2. Restart the process (`node.restart()`)
/// 3. Verify state restoration from redb
pub fn inject_node_kill(node: &mut crate::fixture::NodeProcess) -> Result<InjectionResult, String> {
    let start = Instant::now();
    info!(node = %node.node_id, "injecting L2 failure: node process kill");

    node.kill()?;

    Ok(InjectionResult {
        injected_at: start,
        description: format!("L2: killed node process {}", node.node_id),
    })
}

/// Inject an L2 failure variant: graceful termination with `SIGTERM`.
///
/// Unlike `inject_node_kill`, this allows the node to:
/// - Flush pending billing records
/// - Close NATS connections gracefully
/// - Persist any in-memory state to redb
///
/// Use this to test the graceful shutdown path specifically.
pub fn inject_node_terminate(
    node: &mut crate::fixture::NodeProcess,
) -> Result<InjectionResult, String> {
    let start = Instant::now();
    info!(node = %node.node_id, "injecting L2 failure (graceful): node SIGTERM");

    node.terminate()?;

    Ok(InjectionResult {
        injected_at: start,
        description: format!("L2: SIGTERM node process {}", node.node_id),
    })
}

// ── L3: Redb Corruption ─────────────────────────────────────────────

/// Inject an L3 failure: corrupt a redb page.
///
/// Overwrites bytes in the middle of the redb file to simulate a disk write
/// error, bad sector, or partial write. The corruption targets data pages
/// (second half of the file) to avoid destroying the header, which would
/// make the file unopenable and uninteresting for testing.
///
/// **Important**: The node process must be stopped before calling this.
/// Corrupting a file that redb has open may not produce the expected behavior
/// because the OS may have the data cached in the page cache.
///
/// After corruption, restart the node. The integrity check at startup should
/// detect the corruption and trigger a `PartialRebuild` or `FullRebootstrap`
/// recovery action.
pub fn inject_redb_corruption(db_path: &std::path::Path) -> Result<InjectionResult, String> {
    let start = Instant::now();
    info!(path = %db_path.display(), "injecting L3 failure: redb corruption");

    let file_size = std::fs::metadata(db_path)
        .map_err(|e| format!("failed to read redb file metadata: {e}"))?
        .len();

    if file_size < 16384 {
        return Err(format!(
            "redb file too small to corrupt safely ({} bytes, need ≥16384)",
            file_size
        ));
    }

    // Corrupt a page in the second half of the file (data pages, not header).
    // redb uses 4KB pages. We target the middle of the data region.
    let corrupt_offset = (file_size / 2) as u64;
    let corrupt_data: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(db_path)
        .map_err(|e| format!("failed to open redb file for corruption: {e}"))?;

    file.seek(SeekFrom::Start(corrupt_offset))
        .map_err(|e| format!("failed to seek in redb file: {e}"))?;
    file.write_all(&corrupt_data)
        .map_err(|e| format!("failed to write corrupt data: {e}"))?;
    file.flush()
        .map_err(|e| format!("failed to flush corrupt data: {e}"))?;

    // Also corrupt a second location for thoroughness — target a different page
    let second_offset = corrupt_offset + 8192;
    if second_offset < file_size {
        file.seek(SeekFrom::Start(second_offset))
            .map_err(|e| format!("failed to seek to second corruption point: {e}"))?;
        file.write_all(&[0xFF; 64])
            .map_err(|e| format!("failed to write second corrupt data: {e}"))?;
        file.flush()
            .map_err(|e| format!("failed to flush second corrupt data: {e}"))?;
    }

    // Drop the file handle to release any OS-level locks
    drop(file);

    info!(
        offset = corrupt_offset,
        file_size, "L3 redb corruption injected"
    );

    Ok(InjectionResult {
        injected_at: start,
        description: format!(
            "L3: corrupted redb at offset {corrupt_offset} (file size {file_size})"
        ),
    })
}

// ── L5: NATS Partition ───────────────────────────────────────────────

/// Inject an L5 failure: network partition (NATS disconnection).
///
/// Uses `tc netem` (traffic control network emulator) to drop all packets
/// to the NATS server. If `tc` fails (e.g., missing `CAP_NET_ADMIN`),
/// falls back to `iptables` OUTPUT rule.
///
/// **Requires**: Linux host with `CAP_NET_ADMIN` (WSL or native Linux).
/// The test runner must have permission to run `tc` and `iptables`.
///
/// After calling this, the node should:
/// 1. Detect the NATS disconnection via `NatsHealthWatcher`
/// 2. Enter degraded mode (continue serving existing apps)
/// 3. Stop receiving new deployment events
///
/// Call `remove_nats_partition` to restore connectivity.
pub async fn inject_nats_partition(
    nats_ip: &str,
    nats_port: u16,
) -> Result<InjectionResult, String> {
    let start = Instant::now();
    info!(nats = %nats_ip, port = nats_port, "injecting L5 failure: NATS partition");

    // Register cleanup handler in case the test is interrupted (Ctrl+C)
    register_cleanup(nats_ip, nats_port);

    // Strategy 1: Use `tc qdisc` to add 100% packet loss on loopback
    let tc_output = std::process::Command::new("tc")
        .args([
            "qdisc", "add", "dev", "lo", "root", "handle", "1:", "netem", "loss", "100%",
        ])
        .output();

    match tc_output {
        Ok(output) if output.status.success() => {
            info!("NATS partition injected via tc netem (100% loss on lo)");
            return Ok(InjectionResult {
                injected_at: start,
                description: format!("L5: NATS partition to {nats_ip}:{nats_port} (tc netem)"),
            });
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(%stderr, "tc qdisc add failed, trying iptables fallback");
        }
        Err(e) => {
            warn!(error = %e, "tc command not found, trying iptables fallback");
        }
    }

    // Strategy 2: Use `iptables` to block outbound traffic to NATS
    let ipt_output = std::process::Command::new("iptables")
        .args([
            "-A",
            "OUTPUT",
            "-d",
            nats_ip,
            "-p",
            "tcp",
            "--dport",
            &nats_port.to_string(),
            "-j",
            "DROP",
        ])
        .output()
        .map_err(|e| format!("failed to run iptables: {e}"))?;

    if !ipt_output.status.success() {
        let stderr = String::from_utf8_lossy(&ipt_output.stderr);
        return Err(format!(
            "failed to inject NATS partition (both tc and iptables failed). \
             tc stderr was logged above. iptables stderr: {stderr}. \
             Ensure you have CAP_NET_ADMIN or run as root."
        ));
    }

    info!("NATS partition injected via iptables DROP rule");

    Ok(InjectionResult {
        injected_at: start,
        description: format!("L5: NATS partition to {nats_ip}:{nats_port} (iptables)"),
    })
}

/// Remove the NATS partition (restore connectivity).
///
/// Tries to remove both `tc` and `iptables` rules to ensure connectivity
/// is restored regardless of which method was used for injection.
pub async fn remove_nats_partition(nats_ip: &str, nats_port: u16) -> Result<(), String> {
    info!(nats = %nats_ip, "removing NATS partition");

    // Try removing tc qdisc first
    let tc_result = std::process::Command::new("tc")
        .args(["qdisc", "del", "dev", "lo", "root", "handle", "1:"])
        .output();

    match tc_result {
        Ok(output) if output.status.success() => {
            info!("NATS partition removed via tc qdisc del");
            return Ok(());
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(%stderr, "tc qdisc del failed, trying iptables cleanup");
        }
        Err(e) => {
            warn!(error = %e, "tc command not found for cleanup, trying iptables");
        }
    }

    // Fall back to iptables rule removal
    let ipt_output = std::process::Command::new("iptables")
        .args([
            "-D",
            "OUTPUT",
            "-d",
            nats_ip,
            "-p",
            "tcp",
            "--dport",
            &nats_port.to_string(),
            "-j",
            "DROP",
        ])
        .output()
        .map_err(|e| format!("failed to remove iptables rule: {e}"))?;

    if !ipt_output.status.success() {
        let stderr = String::from_utf8_lossy(&ipt_output.stderr);
        return Err(format!(
            "failed to remove NATS partition. iptables stderr: {stderr}"
        ));
    }

    info!("NATS partition removed via iptables DELETE");

    Ok(())
}

// ── Extra: Disk I/O Latency ──────────────────────────────────────────

/// Inject disk I/O latency.
///
/// On Linux with root, this could use `dmsetup` to create a delay device.
/// For portability in test environments, we simulate I/O pressure by
/// writing a large temporary file to cause buffer cache pressure, which
/// indirectly increases I/O latency for redb operations.
///
/// The `device` parameter is reserved for future `dmsetup` support.
/// Currently, the simulation uses cache pressure only.
pub async fn inject_disk_latency(
    _device: &str,
    latency_ms: u64,
) -> Result<InjectionResult, String> {
    let start = Instant::now();
    info!(latency_ms, "injecting disk I/O latency");

    // Simulate by writing a large file to cause buffer cache pressure.
    // This forces the kernel to flush dirty pages, increasing I/O latency
    // for other processes (including redb).
    let temp_file = std::env::temp_dir().join("chaos_disk_pressure.dat");
    let data = vec![0u8; 1024 * 1024 * 100]; // 100 MB
    std::fs::write(&temp_file, &data)
        .map_err(|e| format!("failed to write disk pressure file: {e}"))?;

    // Force fsync to ensure data hits disk
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&temp_file)
            .map_err(|e| format!("failed to open pressure file for fsync: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("failed to fsync pressure file: {e}"))?;
    }

    info!(
        latency_ms,
        "disk latency injection applied (simulated via cache pressure)"
    );

    Ok(InjectionResult {
        injected_at: start,
        description: format!(
            "disk latency injection: {latency_ms}ms (simulated via cache pressure)"
        ),
    })
}

/// Clean up disk latency injection artifacts.
pub fn remove_disk_latency() -> Result<(), String> {
    let temp_file = std::env::temp_dir().join("chaos_disk_pressure.dat");
    match std::fs::remove_file(&temp_file) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove disk pressure file: {e}")),
    }
}

// ── Extra: Memory Pressure ──────────────────────────────────────────

/// Inject memory pressure by allocating a large amount of memory.
///
/// Spawns a background thread that allocates `target_mb` megabytes in 1 MB
/// increments, holding the allocation for the specified `duration`. This
/// triggers the kernel's memory reclaim and potentially the OOM killer.
///
/// The allocated memory is automatically freed when the duration expires
/// and the background thread exits.
pub async fn inject_memory_pressure(
    target_mb: usize,
    duration: Duration,
) -> Result<InjectionResult, String> {
    let start = Instant::now();
    info!(target_mb, ?duration, "injecting memory pressure");

    tokio::task::spawn_blocking(move || {
        let mut buffers: Vec<Vec<u8>> = Vec::with_capacity(target_mb);
        let chunk_size = 1024 * 1024; // 1 MB

        for i in 0..target_mb {
            // Allocate and touch pages to ensure physical memory is committed
            let mut buf = vec![0u8; chunk_size];
            // Touch each page (4KB) to ensure the kernel actually allocates physical pages
            for offset in (0..chunk_size).step_by(4096) {
                buf[offset] = 0xAA;
            }
            buffers.push(buf);

            // Small delay between allocations to allow the kernel to react
            if i % 100 == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        info!(
            allocated_mb = target_mb,
            "memory pressure allocated, holding for {:?}", duration
        );

        // Hold the memory for the specified duration
        std::thread::sleep(duration);

        // Memory is freed when `buffers` goes out of scope
        info!("memory pressure released");
    });

    // Give the allocation a moment to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(InjectionResult {
        injected_at: start,
        description: format!("memory pressure: {target_mb} MB for {:?}", duration),
    })
}

// ── Cleanup ──────────────────────────────────────────────────────────

/// Register a Ctrl+C handler to clean up network rules if the test is
/// interrupted during a NATS partition injection.
///
/// Without this, `iptables` DROP rules may persist after the test process
/// exits, breaking subsequent tests or even normal system operation.
fn register_cleanup(nats_ip: &str, nats_port: u16) {
    let ip = nats_ip.to_string();
    let port = nats_port;

    // Best-effort: if a handler is already set, this will fail silently.
    // That's acceptable because only one partition test runs at a time
    // (--test-threads=1).
    let _ = ctrlc::set_handler(move || {
        warn!("Ctrl+C received during chaos test — cleaning up network rules");

        // Best-effort cleanup of iptables rules
        let _ = std::process::Command::new("iptables")
            .args([
                "-D",
                "OUTPUT",
                "-d",
                &ip,
                "-p",
                "tcp",
                "--dport",
                &port.to_string(),
                "-j",
                "DROP",
            ])
            .output();

        // Best-effort cleanup of tc qdisc
        let _ = std::process::Command::new("tc")
            .args(["qdisc", "del", "dev", "lo", "root", "handle", "1:"])
            .output();

        // Best-effort cleanup of disk pressure file
        let temp_file = std::env::temp_dir().join("chaos_disk_pressure.dat");
        let _ = std::fs::remove_file(&temp_file);

        // Exit with the standard Ctrl+C exit code
        std::process::exit(130);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_injection_result_description() {
        let result = InjectionResult {
            injected_at: Instant::now(),
            description: "L1: killed instance of test-app:v1 (was 1 running)".to_string(),
        };
        assert!(result.description.starts_with("L1:"));
    }

    #[test]
    fn test_redb_corruption_rejects_small_file() {
        // Create a tiny temp file
        let dir = tempfile::tempdir().unwrap();
        let tiny_file = dir.path().join("tiny.redb");
        std::fs::write(&tiny_file, b"hello").unwrap();

        let result = inject_redb_corruption(&tiny_file);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too small"));
    }

    #[test]
    fn test_redb_corruption_succeeds_on_large_file() {
        // Create a file large enough to corrupt
        let dir = tempfile::tempdir().unwrap();
        let large_file = dir.path().join("large.redb");
        let data = vec![0u8; 32768]; // 32 KB
        std::fs::write(&large_file, &data).unwrap();

        let result = inject_redb_corruption(&large_file);
        assert!(result.is_ok());

        // Verify the file was actually modified
        let modified = std::fs::read(&large_file).unwrap();
        // The corruption marker bytes should be present in the second half
        let second_half = &modified[16384..];
        assert!(second_half
            .windows(8)
            .any(|w| w == [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]));
    }

    #[test]
    fn test_remove_disk_latency_cleans_up() {
        // Create the pressure file
        let temp_file = std::env::temp_dir().join("chaos_disk_pressure.dat");
        std::fs::write(&temp_file, b"test").unwrap();
        assert!(temp_file.exists());

        // Clean it up
        remove_disk_latency().unwrap();
        assert!(!temp_file.exists());
    }

    #[test]
    fn test_remove_disk_latency_idempotent() {
        // Should not error if the file doesn't exist
        let temp_file = std::env::temp_dir().join("chaos_disk_pressure.dat");
        let _ = std::fs::remove_file(&temp_file);
        assert!(remove_disk_latency().is_ok());
    }
}
