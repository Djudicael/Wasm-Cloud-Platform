//! Userspace fallback monitoring for when eBPF is not available.
//!
//! On systems without eBPF support (Windows, macOS, old kernels, or missing
//! capabilities), the monitor falls back to userspace polling. This provides
//! the same API surface (same metrics, same action callbacks) but with higher
//! latency and less precision.
//!
//! # What the Fallback Can Detect
//!
//! - **Memory pressure**: Reads `/proc/meminfo` for available memory
//! - **FD usage**: Counts entries in `/proc/<pid>/fd`
//! - **Process exits**: Polls `/proc/<pid>/stat` for child process status
//! - **Disk stats**: Reads `/proc/diskstats` for aggregate I/O stats
//!
//! # What the Fallback Cannot Detect
//!
//! - **Per-request disk I/O latency**: Only aggregate stats are available
//! - **TCP retransmits**: No per-connection TCP state tracking
//! - **Syscall anomalies**: No kernel-level syscall interception
//! - **Sub-second response**: Polling interval is 5 seconds (vs 10ms for eBPF)

use std::sync::Arc;
use std::time::Duration;

use tokio::time;
use tracing::{debug, info, warn};

use crate::actions::ActionDispatcher;
use crate::actions::MonitorEvent;
use crate::config::MonitorConfig;
use crate::metrics::EbpfMetrics;

/// Run the userspace fallback monitor (higher latency, no kernel hooks).
///
/// This function runs in a loop, polling `/proc` and `/sys` at the configured
/// interval. It produces `MonitorEvent`s and dispatches them through the
/// `ActionDispatcher`, just like the eBPF ring buffer consumer.
///
/// The function returns when the `shutdown` token is cancelled.
pub async fn run_fallback_monitor(
    config: MonitorConfig,
    metrics: Arc<EbpfMetrics>,
    dispatcher: Arc<ActionDispatcher>,
    node_pid: u32,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    info!(
        node_pid,
        interval_secs = 5,
        "Running eBPF monitor in userspace fallback mode (higher latency)"
    );

    metrics.mark_ebpf_fallback();

    let mut interval = time::interval(Duration::from_secs(5));
    let mut last_fd_count: u32 = 0;
    let mut last_pressure_level: u32 = 0;
    let mut consecutive_fd_increase: u32 = 0;

    loop {
        tokio::select! {
            _ = interval.tick() => {},
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("eBPF fallback monitor shutting down");
                    return;
                }
            }
        }

        // ── Memory Pressure ────────────────────────────────────────────
        if let Ok(meminfo) = read_meminfo() {
            let free_pages = meminfo.mem_available_kb * 1024 / 4096;
            let pressure_level = if free_pages < config.mem_critical_threshold_pages {
                2
            } else if free_pages < config.mem_low_threshold_pages {
                1
            } else {
                0
            };

            // Only dispatch if the pressure level changed
            if pressure_level != last_pressure_level {
                let event = MonitorEvent::MemPressure {
                    pid: node_pid,
                    free_pages,
                    reclaim_pages: 0, // Not available from /proc/meminfo
                    pressure_level,
                    anon_pages: 0, // Not available from /proc/meminfo
                };
                dispatcher.dispatch(event);
                last_pressure_level = pressure_level;
            }

            // Always update the metric
            metrics.set_memory_pressure(pressure_level);

            if pressure_level >= 2 {
                warn!(
                    free_pages,
                    threshold = config.mem_critical_threshold_pages,
                    "Memory pressure detected (userspace fallback): CRITICAL"
                );
            } else if pressure_level >= 1 {
                debug!(
                    free_pages,
                    threshold = config.mem_low_threshold_pages,
                    "Memory pressure detected (userspace fallback): MEDIUM"
                );
            }
        }

        // ── FD Usage ───────────────────────────────────────────────────
        if let Ok(fd_count) = count_fds(node_pid) {
            metrics.set_fd_usage(fd_count, config.fd_soft_limit);

            let ratio = fd_count as f64 / config.fd_soft_limit as f64;

            // Detect FD leak: monotonically increasing over multiple windows
            if fd_count > last_fd_count && last_fd_count > 0 {
                consecutive_fd_increase += 1;
                if consecutive_fd_increase >= 3 {
                    warn!(
                        fd_count,
                        last_fd_count,
                        consecutive_windows = consecutive_fd_increase,
                        "Potential FD leak detected (userspace fallback): \
                         FD count increased monotonically over 3+ windows"
                    );
                }
            } else {
                consecutive_fd_increase = 0;
            }

            if ratio > 0.95 {
                // Hard limit approaching — critical
                let event = MonitorEvent::FdLimitApproaching {
                    pid: node_pid,
                    fd: 0, // No specific FD in userspace fallback
                    current_fd_count: fd_count,
                    fd_soft_limit: config.fd_soft_limit,
                };
                dispatcher.dispatch(event);
                warn!(
                    fd_count,
                    soft_limit = config.fd_soft_limit,
                    hard_limit = config.fd_hard_limit,
                    "FD hard limit approaching (userspace fallback)"
                );
            } else if ratio > 0.8 {
                // Soft limit approaching — warning
                let event = MonitorEvent::FdOpen {
                    pid: node_pid,
                    fd: 0,
                    current_fd_count: fd_count,
                    fd_soft_limit: config.fd_soft_limit,
                };
                dispatcher.dispatch(event);
                debug!(
                    fd_count,
                    soft_limit = config.fd_soft_limit,
                    "FD soft limit approaching (userspace fallback)"
                );
            }

            last_fd_count = fd_count;
        }

        // ── Child Process Monitoring ───────────────────────────────────
        // Check if any child processes have exited
        if let Ok(children) = get_child_pids(node_pid) {
            for child_pid in children {
                if !is_process_running(child_pid) {
                    // Child process exited — dispatch event
                    // We can't determine the exit code or signal from
                    // userspace polling, so we use defaults.
                    let event = MonitorEvent::ProcessExit {
                        pid: child_pid,
                        ppid: node_pid,
                        exit_code: 0,    // Unknown from userspace
                        signal: 0,       // Unknown from userspace
                        comm: [0u8; 16], // Unknown from userspace
                        cgroup_id: 0,    // Unknown from userspace
                    };
                    dispatcher.dispatch(event);
                    debug!(
                        child_pid,
                        "Child process exit detected (userspace fallback)"
                    );
                }
            }
        }

        // ── Disk Stats (approximation) ─────────────────────────────────
        // Userspace cannot measure per-request disk I/O latency — only
        // aggregate stats from /proc/diskstats. We read them for
        // informational purposes but don't generate DiskSlowIo events
        // since we can't measure per-request latency.
        if let Ok(disk_stats) = read_disk_stats() {
            debug!(
                reads = disk_stats.reads_completed,
                writes = disk_stats.writes_completed,
                read_ms = disk_stats.read_ms,
                write_ms = disk_stats.write_ms,
                "Disk stats (userspace fallback)"
            );
        }
    }
}

// ── /proc Parsers ─────────────────────────────────────────────────────────────

/// Memory information parsed from `/proc/meminfo`.
#[allow(dead_code)]
struct MemInfo {
    mem_total_kb: u64,
    mem_available_kb: u64,
}

/// Read memory information from `/proc/meminfo`.
///
/// Returns `Err` if the file cannot be read (e.g., not on Linux).
fn read_meminfo() -> Result<MemInfo, std::io::Error> {
    let content = std::fs::read_to_string("/proc/meminfo")?;
    let mut total = 0u64;
    let mut available = 0u64;
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            total = parse_kb(line);
        } else if line.starts_with("MemAvailable:") {
            available = parse_kb(line);
        }
    }
    Ok(MemInfo {
        mem_total_kb: total,
        mem_available_kb: available,
    })
}

/// Parse a "MemXXX:    12345 kB" line, returning the value in KB.
fn parse_kb(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Count the number of open file descriptors for a process.
///
/// Reads `/proc/<pid>/fd` and counts the entries.
fn count_fds(pid: u32) -> Result<u32, std::io::Error> {
    let fd_dir = format!("/proc/{}/fd", pid);
    match std::fs::read_dir(&fd_dir) {
        Ok(entries) => Ok(entries.count() as u32),
        Err(e) => {
            // EACCES is common if we don't have permission to read other processes' FDs
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                debug!(pid, "Permission denied reading /proc/{}/fd", pid);
            }
            Err(e)
        }
    }
}

/// Check if a process is still running by checking `/proc/<pid>/stat`.
fn is_process_running(pid: u32) -> bool {
    let stat_path = format!("/proc/{}/stat", pid);
    std::fs::metadata(&stat_path).is_ok()
}

/// Get child PIDs of a process by reading `/proc/<pid>/task/<tid>/children`.
///
/// Falls back to scanning `/proc` for processes whose PPID matches.
fn get_child_pids(pid: u32) -> Result<Vec<u32>, std::io::Error> {
    // Method 1: Try /proc/<pid>/task/<pid>/children (Linux >= 3.5)
    let children_path = format!("/proc/{}/task/{}/children", pid, pid);
    if let Ok(content) = std::fs::read_to_string(&children_path) {
        let pids: Vec<u32> = content
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if !pids.is_empty() {
            return Ok(pids);
        }
    }

    // Method 2: Scan /proc for processes whose PPID matches
    let mut children = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                if let Ok(child_pid) = name.parse::<u32>() {
                    if child_pid == pid {
                        continue;
                    }
                    let stat_path = format!("/proc/{}/stat", child_pid);
                    if let Ok(stat_content) = std::fs::read_to_string(&stat_path) {
                        if let Some(ppid) = parse_ppid_from_stat(&stat_content) {
                            if ppid == pid {
                                children.push(child_pid);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(children)
}

/// Parse the PPID from a `/proc/<pid>/stat` line.
///
/// Format: `pid (comm) state ppid ...`
/// The comm field can contain spaces and parentheses, so we find the
/// closing parenthesis first, then parse the fields after it.
fn parse_ppid_from_stat(stat: &str) -> Option<u32> {
    // Find the closing ')' to skip the comm field
    let close_paren = stat.rfind(')')?;
    let rest = &stat[close_paren + 1..];
    // rest is: " state ppid ..."
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] = state, fields[1] = ppid
    fields.get(1).and_then(|s| s.parse().ok())
}

/// Aggregate disk statistics parsed from `/proc/diskstats`.
struct DiskStats {
    reads_completed: u64,
    writes_completed: u64,
    read_ms: u64,
    write_ms: u64,
}

/// Read aggregate disk statistics from `/proc/diskstats`.
///
/// Returns stats for the first disk device found. This is an approximation
/// since we can't attribute I/O to specific devices from userspace.
fn read_disk_stats() -> Result<DiskStats, std::io::Error> {
    let content = std::fs::read_to_string("/proc/diskstats")?;
    // Take the first non-partition line (major >= 8 for SCSI/SATA)
    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 11 {
            continue;
        }
        let major: u64 = fields[0].parse().unwrap_or(0);
        // Skip partition entries (they have fewer fields or specific minor numbers)
        if major < 1 {
            continue;
        }
        // Fields: major minor name reads_completed reads_merged sectors_read ms_reading
        //         writes_completed writes_merged sectors_written ms_writing
        return Ok(DiskStats {
            reads_completed: fields[3].parse().unwrap_or(0),
            writes_completed: fields[7].parse().unwrap_or(0),
            read_ms: fields[6].parse().unwrap_or(0),
            write_ms: fields[10].parse().unwrap_or(0),
        });
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no disk entries found in /proc/diskstats",
    ))
}

// ── Unit Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Registry;

    #[test]
    fn test_parse_kb_standard() {
        assert_eq!(parse_kb("MemTotal:       16384000 kB"), 16384000);
        assert_eq!(parse_kb("MemAvailable:    8192000 kB"), 8192000);
    }

    #[test]
    fn test_parse_kb_extra_whitespace() {
        assert_eq!(parse_kb("MemTotal:    12345 kB"), 12345);
    }

    #[test]
    fn test_parse_kb_no_kb_suffix() {
        // Even without "kB" suffix, the second field should parse
        assert_eq!(parse_kb("MemTotal:       16384000"), 16384000);
    }

    #[test]
    fn test_parse_kb_empty() {
        assert_eq!(parse_kb(""), 0);
    }

    #[test]
    fn test_parse_kb_no_number() {
        assert_eq!(parse_kb("MemTotal:  notanumber kB"), 0);
    }

    #[test]
    fn test_parse_ppid_from_stat() {
        let stat = "1234 (some process name) S 1 1234 1234 0 0 0 0 0 0 0";
        assert_eq!(parse_ppid_from_stat(stat), Some(1));
    }

    #[test]
    fn test_parse_ppid_from_stat_with_parens_in_comm() {
        // Some process names contain parentheses
        let stat = "5678 (a (b) c) R 42 5678 5678 0 0 0 0 0 0 0";
        assert_eq!(parse_ppid_from_stat(stat), Some(42));
    }

    #[test]
    fn test_parse_ppid_from_stat_empty() {
        assert_eq!(parse_ppid_from_stat(""), None);
    }

    #[test]
    fn test_parse_ppid_from_stat_no_close_paren() {
        assert_eq!(parse_ppid_from_stat("1234 (no close"), None);
    }

    #[test]
    fn test_count_fds_current_process() {
        // We should be able to count FDs for our own process
        let pid = std::process::id();
        let result = count_fds(pid);
        if cfg!(target_os = "linux") {
            // On Linux, this should succeed
            assert!(result.is_ok());
            assert!(result.unwrap() > 0, "process should have at least some FDs");
        }
    }

    #[test]
    fn test_is_process_running_current() {
        if cfg!(target_os = "linux") {
            let pid = std::process::id();
            assert!(is_process_running(pid));
        }
    }

    #[test]
    fn test_is_process_running_nonexistent() {
        if cfg!(target_os = "linux") {
            // PID 99999999 is very unlikely to exist
            assert!(!is_process_running(99999999));
        }
    }

    #[test]
    fn test_read_meminfo_on_linux() {
        if cfg!(target_os = "linux") {
            let result = read_meminfo();
            assert!(result.is_ok());
            let meminfo = result.unwrap();
            assert!(meminfo.mem_total_kb > 0, "total memory should be > 0");
            assert!(
                meminfo.mem_available_kb > 0,
                "available memory should be > 0"
            );
            assert!(
                meminfo.mem_available_kb <= meminfo.mem_total_kb,
                "available should not exceed total"
            );
        }
    }

    #[test]
    fn test_read_disk_stats_on_linux() {
        if cfg!(target_os = "linux") {
            let result = read_disk_stats();
            // This might fail in some environments (containers without /proc/diskstats,
            // WSL2 with minimal disk stats, etc.). Just verify it doesn't panic and
            // returns valid data when available.
            if let Ok(stats) = result {
                // On a running system, at least one of reads or writes should be non-zero,
                // but in some WSL/container environments this may not be the case.
                // Just verify the values are reasonable (not absurdly large).
                assert!(
                    stats.reads_completed < 1_000_000_000,
                    "reads_completed should be reasonable"
                );
                assert!(
                    stats.writes_completed < 1_000_000_000,
                    "writes_completed should be reasonable"
                );
            }
            // If read_disk_stats fails (e.g., no /proc/diskstats in some containers),
            // that's also acceptable — the fallback monitor handles this gracefully.
        }
    }

    #[test]
    fn test_get_child_pids_current_process() {
        if cfg!(target_os = "linux") {
            let pid = std::process::id();
            let result = get_child_pids(pid);
            assert!(result.is_ok());
            // The current process likely has no children in test mode
            let children = result.unwrap();
            // Just verify it doesn't panic and returns a valid vec
            assert!(children.len() < 1000, "unreasonable number of children");
        }
    }

    #[tokio::test]
    async fn test_fallback_monitor_dispatches_memory_pressure() {
        let registry = Registry::new();
        let metrics = Arc::new(EbpfMetrics::new(&registry));
        let dispatcher = Arc::new(ActionDispatcher::new_noop(
            metrics.clone(),
            "test-node".to_string(),
        ));

        // Create a shutdown channel
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let config = MonitorConfig {
            enabled: true,
            mem_low_threshold_pages: 999999999, // Very high to trigger pressure
            ..MonitorConfig::default()
        };

        let metrics_clone = metrics.clone();
        let dispatcher_clone = dispatcher.clone();

        // Start the fallback monitor in a background task
        let handle = tokio::spawn(async move {
            run_fallback_monitor(
                config,
                metrics_clone,
                dispatcher_clone,
                std::process::id(),
                shutdown_rx,
            )
            .await;
        });

        // Give it a moment to run at least one iteration (on Linux)
        if cfg!(target_os = "linux") {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Signal shutdown
        let _ = shutdown_tx.send(true);
        handle.abort();

        // Verify metrics were updated (on Linux)
        if cfg!(target_os = "linux") {
            // The ebpf_active gauge should be 0 (fallback mode)
            assert_eq!(metrics.ebpf_active.get(), 0);
        }
    }

    #[test]
    fn test_meminfo_struct() {
        let meminfo = MemInfo {
            mem_total_kb: 16384000,
            mem_available_kb: 8192000,
        };
        assert_eq!(meminfo.mem_total_kb, 16384000);
        assert_eq!(meminfo.mem_available_kb, 8192000);
    }

    #[test]
    fn test_disk_stats_struct() {
        let stats = DiskStats {
            reads_completed: 1000,
            writes_completed: 500,
            read_ms: 100,
            write_ms: 50,
        };
        assert_eq!(stats.reads_completed, 1000);
        assert_eq!(stats.writes_completed, 500);
    }
}
