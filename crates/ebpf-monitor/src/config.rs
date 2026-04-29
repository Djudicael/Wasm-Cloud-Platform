//! Configuration for the eBPF monitor.
//!
//! `MonitorConfig` is the operational config used by the eBPF monitor crate.
//! It can be created from the platform's `EbpfSection` (cold config) or
//! constructed manually. It also supports validation and hot-reload updates.

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

    /// Enable namespace enforcer eBPF program.
    pub enable_namespace_enforcer: bool,
    /// Port the internal gateway listens on (for namespace enforcement).
    pub gateway_port: u16,
    /// Enable forged header detection in namespace enforcer.
    pub enable_forged_header_detect: bool,
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
            enable_namespace_enforcer: true,
            gateway_port: common::INTERNAL_GATEWAY_PORT,
            enable_forged_header_detect: true,
        }
    }
}

impl MonitorConfig {
    /// Create a `MonitorConfig` from the platform's `EbpfSection` (cold config).
    ///
    /// This is the primary way to construct a `MonitorConfig` during node
    /// startup. The `EbpfSection` comes from the TOML config file (or defaults),
    /// and the individual program enable flags default to `true`.
    pub fn from_ebpf_section(section: &common::config::EbpfSection) -> Self {
        MonitorConfig {
            enabled: section.enabled,
            fd_soft_limit: section.fd_soft_limit,
            fd_hard_limit: section.fd_hard_limit,
            mem_low_threshold_pages: section.mem_low_threshold_pages,
            mem_critical_threshold_pages: section.mem_critical_threshold_pages,
            disk_slow_threshold_ns: section.disk_slow_threshold_ns,
            tcp_conn_limit_per_pid: section.tcp_conn_limit_per_pid,
            syscall_rate_limit: section.syscall_rate_limit,
            sampling_period_secs: section.sampling_period_secs,
            // Individual program enables default to true.
            // The EbpfSection doesn't have per-program toggles,
            // so we always enable all programs when eBPF is enabled.
            enable_process_tracker: true,
            enable_tcp_monitor: true,
            enable_fd_watcher: true,
            enable_mem_pressure: true,
            enable_disk_monitor: true,
            enable_syscall_counter: true,
            enable_namespace_enforcer: section.enable_namespace_enforcer,
            gateway_port: section.gateway_port,
            enable_forged_header_detect: section.enable_forged_header_detect,
        }
    }

    /// Validate the configuration, returning a list of errors.
    ///
    /// Returns `Ok(())` if the configuration is valid, or `Err` with a
    /// description of all validation failures.
    pub fn validate(&self) -> Result<(), String> {
        let mut errors = Vec::new();

        if self.fd_soft_limit >= self.fd_hard_limit {
            errors.push(format!(
                "fd_soft_limit ({}) must be less than fd_hard_limit ({})",
                self.fd_soft_limit, self.fd_hard_limit
            ));
        }

        if self.fd_soft_limit == 0 {
            errors.push("fd_soft_limit must be > 0".to_string());
        }

        if self.fd_hard_limit == 0 {
            errors.push("fd_hard_limit must be > 0".to_string());
        }

        if self.mem_low_threshold_pages <= self.mem_critical_threshold_pages {
            errors.push(format!(
                "mem_low_threshold_pages ({}) must be greater than mem_critical_threshold_pages ({})",
                self.mem_low_threshold_pages, self.mem_critical_threshold_pages
            ));
        }

        if self.mem_critical_threshold_pages == 0 {
            errors.push("mem_critical_threshold_pages must be > 0".to_string());
        }

        if self.disk_slow_threshold_ns == 0 {
            errors.push("disk_slow_threshold_ns must be > 0".to_string());
        }

        if self.tcp_conn_limit_per_pid == 0 {
            errors.push("tcp_conn_limit_per_pid must be > 0".to_string());
        }

        if self.syscall_rate_limit == 0 {
            errors.push("syscall_rate_limit must be > 0".to_string());
        }

        if self.sampling_period_secs == 0 {
            errors.push("sampling_period_secs must be > 0".to_string());
        }

        if !errors.is_empty() {
            Err(format!(
                "eBPF monitor configuration validation failed:\n  - {}",
                errors.join("\n  - ")
            ))
        } else {
            Ok(())
        }
    }

    /// Apply a hot-reload update from an `EbpfSection`.
    ///
    /// Only the threshold fields are updated; the `enabled` flag and
    /// per-program toggles are not changed at runtime (they require
    /// eBPF program load/unload which is a cold operation).
    pub fn sync_from_ebpf_section(&mut self, section: &common::config::EbpfSection) {
        self.fd_soft_limit = section.fd_soft_limit;
        self.fd_hard_limit = section.fd_hard_limit;
        self.mem_low_threshold_pages = section.mem_low_threshold_pages;
        self.mem_critical_threshold_pages = section.mem_critical_threshold_pages;
        self.disk_slow_threshold_ns = section.disk_slow_threshold_ns;
        self.tcp_conn_limit_per_pid = section.tcp_conn_limit_per_pid;
        self.syscall_rate_limit = section.syscall_rate_limit;
        self.sampling_period_secs = section.sampling_period_secs;
    }

    /// Count how many eBPF programs are enabled.
    pub fn enabled_program_count(&self) -> usize {
        let mut count = 0;
        if self.enable_process_tracker {
            count += 1;
        }
        if self.enable_tcp_monitor {
            count += 1;
        }
        if self.enable_fd_watcher {
            count += 1;
        }
        if self.enable_mem_pressure {
            count += 1;
        }
        if self.enable_disk_monitor {
            count += 1;
        }
        if self.enable_syscall_counter {
            count += 1;
        }
        if self.enable_namespace_enforcer {
            count += 1;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_valid() {
        let config = MonitorConfig::default();
        assert!(config.validate().is_ok(), "default config should be valid");
    }

    #[test]
    fn test_validate_fd_soft_ge_hard() {
        let config = MonitorConfig {
            fd_soft_limit: 10000,
            fd_hard_limit: 9000,
            ..MonitorConfig::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("fd_soft_limit"));
    }

    #[test]
    fn test_validate_fd_soft_eq_hard() {
        let config = MonitorConfig {
            fd_soft_limit: 8192,
            fd_hard_limit: 8192,
            ..MonitorConfig::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("fd_soft_limit"));
    }

    #[test]
    fn test_validate_mem_low_le_critical() {
        let config = MonitorConfig {
            mem_low_threshold_pages: 10000,
            mem_critical_threshold_pages: 20000,
            ..MonitorConfig::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("mem_low_threshold_pages"));
    }

    #[test]
    fn test_validate_zero_values() {
        let config = MonitorConfig {
            fd_soft_limit: 0,
            fd_hard_limit: 0,
            mem_critical_threshold_pages: 0,
            disk_slow_threshold_ns: 0,
            tcp_conn_limit_per_pid: 0,
            syscall_rate_limit: 0,
            sampling_period_secs: 0,
            ..MonitorConfig::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("fd_soft_limit"));
        assert!(err.contains("fd_hard_limit"));
        assert!(err.contains("mem_critical_threshold_pages"));
        assert!(err.contains("disk_slow_threshold_ns"));
        assert!(err.contains("tcp_conn_limit_per_pid"));
        assert!(err.contains("syscall_rate_limit"));
        assert!(err.contains("sampling_period_secs"));
    }

    #[test]
    fn test_validate_valid_config() {
        let config = MonitorConfig {
            enabled: true,
            fd_soft_limit: 8192,
            fd_hard_limit: 9728,
            mem_low_threshold_pages: 65536,
            mem_critical_threshold_pages: 16384,
            disk_slow_threshold_ns: 50_000_000,
            tcp_conn_limit_per_pid: 10000,
            syscall_rate_limit: 100_000,
            sampling_period_secs: 10,
            enable_process_tracker: true,
            enable_tcp_monitor: true,
            enable_fd_watcher: true,
            enable_mem_pressure: true,
            enable_disk_monitor: true,
            enable_syscall_counter: true,
            enable_namespace_enforcer: true,
            gateway_port: common::INTERNAL_GATEWAY_PORT,
            enable_forged_header_detect: true,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_from_ebpf_section() {
        let section = common::config::EbpfSection::default();
        let config = MonitorConfig::from_ebpf_section(&section);
        assert_eq!(config.enabled, section.enabled);
        assert_eq!(config.fd_soft_limit, section.fd_soft_limit);
        assert_eq!(config.fd_hard_limit, section.fd_hard_limit);
        assert_eq!(
            config.mem_low_threshold_pages,
            section.mem_low_threshold_pages
        );
        assert_eq!(
            config.mem_critical_threshold_pages,
            section.mem_critical_threshold_pages
        );
        assert_eq!(
            config.disk_slow_threshold_ns,
            section.disk_slow_threshold_ns
        );
        assert_eq!(
            config.tcp_conn_limit_per_pid,
            section.tcp_conn_limit_per_pid
        );
        assert_eq!(config.syscall_rate_limit, section.syscall_rate_limit);
        assert_eq!(config.sampling_period_secs, section.sampling_period_secs);
        // Per-program enables should default to true
        assert!(config.enable_process_tracker);
        assert!(config.enable_tcp_monitor);
        assert!(config.enable_fd_watcher);
        assert!(config.enable_mem_pressure);
        assert!(config.enable_disk_monitor);
        assert!(config.enable_syscall_counter);
        assert!(config.enable_namespace_enforcer);
        assert_eq!(config.gateway_port, common::INTERNAL_GATEWAY_PORT);
        assert!(config.enable_forged_header_detect);
    }

    #[test]
    fn test_sync_from_ebpf_section() {
        let mut config = MonitorConfig::default();
        let mut section = common::config::EbpfSection::default();
        section.fd_soft_limit = 4096;
        section.fd_hard_limit = 5000;
        section.syscall_rate_limit = 50_000;

        config.sync_from_ebpf_section(&section);
        assert_eq!(config.fd_soft_limit, 4096);
        assert_eq!(config.fd_hard_limit, 5000);
        assert_eq!(config.syscall_rate_limit, 50_000);
        // enabled and per-program flags should NOT change
        assert!(config.enabled);
        assert!(config.enable_process_tracker);
    }

    #[test]
    fn test_enabled_program_count_all() {
        let config = MonitorConfig::default();
        assert_eq!(config.enabled_program_count(), 7);
    }

    #[test]
    fn test_enabled_program_count_partial() {
        let config = MonitorConfig {
            enable_process_tracker: true,
            enable_tcp_monitor: false,
            enable_fd_watcher: true,
            enable_mem_pressure: false,
            enable_disk_monitor: false,
            enable_syscall_counter: true,
            enable_namespace_enforcer: false,
            ..MonitorConfig::default()
        };
        assert_eq!(config.enabled_program_count(), 3);
    }

    #[test]
    fn test_enabled_program_count_none() {
        let config = MonitorConfig {
            enable_process_tracker: false,
            enable_tcp_monitor: false,
            enable_fd_watcher: false,
            enable_mem_pressure: false,
            enable_disk_monitor: false,
            enable_syscall_counter: false,
            enable_namespace_enforcer: false,
            ..MonitorConfig::default()
        };
        assert_eq!(config.enabled_program_count(), 0);
    }
}
