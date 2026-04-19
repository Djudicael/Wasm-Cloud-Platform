//! Configuration for the eBPF monitor.

use serde::{Deserialize, Serialize};

/// Configuration for the eBPF monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// Enable eBPF monitoring (requires Linux kernel >= 5.8).
    pub enabled: bool,

    /// FD soft limit per Wasm instance (warning at 80%).
    pub fd_soft_limit: u32,

    /// FD hard limit per Wasm instance (kill at 95%).
    pub fd_hard_limit: u32,

    /// Memory pressure low threshold (free pages).
    pub mem_low_threshold_pages: u64,

    /// Memory pressure critical threshold (free pages).
    pub mem_critical_threshold_pages: u64,

    /// Disk I/O latency threshold for "slow" alert (nanoseconds).
    pub disk_slow_threshold_ns: u64,

    /// Maximum TCP connections per PID before alert.
    pub tcp_conn_limit_per_pid: u32,

    /// Syscall rate limit per second for suspicious categories.
    pub syscall_rate_limit: u64,

    /// Sampling period for periodic counters (seconds).
    pub sampling_period_secs: u64,

    /// Enable individual eBPF programs.
    pub enable_process_tracker: bool,
    pub enable_tcp_monitor: bool,
    pub enable_fd_watcher: bool,
    pub enable_mem_pressure: bool,
    pub enable_disk_monitor: bool,
    pub enable_syscall_counter: bool,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        MonitorConfig {
            enabled: true,
            fd_soft_limit: 8192,                 // 80% of default 1024 soft limit
            fd_hard_limit: 9728,                 // 95% of 10240
            mem_low_threshold_pages: 65536,      // ~256 MB free
            mem_critical_threshold_pages: 16384, // ~64 MB free
            disk_slow_threshold_ns: 50_000_000,  // 50 ms
            tcp_conn_limit_per_pid: 10000,
            syscall_rate_limit: 100_000,
            sampling_period_secs: 10,
            enable_process_tracker: true,
            enable_tcp_monitor: true,
            enable_fd_watcher: true,
            enable_mem_pressure: true,
            enable_disk_monitor: true,
            enable_syscall_counter: true,
        }
    }
}
