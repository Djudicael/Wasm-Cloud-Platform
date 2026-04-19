//! Userspace fallback monitoring for when eBPF is not available.

use std::time::Duration;
use tokio::time;
use tracing::{info, warn};

use crate::config::MonitorConfig;

/// Run the userspace fallback monitor (higher latency, no kernel hooks).
pub async fn run_fallback_monitor(config: MonitorConfig) {
    info!("Running eBPF monitor in userspace fallback mode (higher latency)");

    let node_pid = std::process::id();
    let mut interval = time::interval(Duration::from_secs(5)); // Same as health loop

    loop {
        interval.tick().await;

        // Read /proc/meminfo for memory pressure
        if let Ok(meminfo) = read_meminfo() {
            let free_pages = meminfo.mem_available_kb * 1024 / 4096;
            let pressure_level = if free_pages < config.mem_critical_threshold_pages {
                2
            } else if free_pages < config.mem_low_threshold_pages {
                1
            } else {
                0
            };

            if pressure_level >= 2 {
                warn!(
                    "Memory pressure detected (userspace fallback): free_pages={}",
                    free_pages
                );
            }
        }

        // Read /proc/<pid>/fd for FD count
        if let Ok(fd_count) = count_fds(node_pid) {
            let ratio = fd_count as f64 / config.fd_soft_limit as f64;

            if ratio > 0.9 {
                warn!(
                    "FD usage high (userspace fallback): {}/{}",
                    fd_count, config.fd_soft_limit
                );
            }
        }

        // Note: Userspace cannot measure per‑request disk I/O latency — only aggregate stats
        // We'll rely on existing Prometheus disk metrics for that.
    }
}

struct MemInfo {
    mem_total_kb: u64,
    mem_available_kb: u64,
}

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

fn parse_kb(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn count_fds(pid: u32) -> Result<u32, std::io::Error> {
    let fd_dir = format!("/proc/{}/fd", pid);
    Ok(std::fs::read_dir(fd_dir)?.count() as u32)
}
