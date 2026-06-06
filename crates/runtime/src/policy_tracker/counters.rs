use std::sync::atomic::{AtomicU32, AtomicU64};

/// Atomic counters for a single instance's policy enforcement.
/// Shared between the WASI host functions and the metrics exporter.
#[derive(Debug)]
pub struct PolicyCounters {
    // Network counters
    pub outbound_connections_active: AtomicU32,
    pub outbound_connections_total: AtomicU64,
    pub egress_bytes: AtomicU64,
    pub dns_lookups_total: AtomicU64,
    pub inbound_connections_active: AtomicU32,

    // Runtime resource limiter counters
    pub current_memory_bytes: AtomicU64,
    pub memory_bytes_peak: AtomicU64,
    pub current_table_elements: AtomicU32,
    pub table_elements_peak: AtomicU32,

    // Filesystem counters
    pub open_fds: AtomicU32,
    pub open_fds_peak: AtomicU32,
    pub fd_open_total: AtomicU64,
    pub fs_write_bytes: AtomicU64,
    pub fs_read_bytes: AtomicU64,
    pub file_creates_total: AtomicU64,
    pub file_deletes_total: AtomicU64,

    // Violation counters
    pub connection_denied_total: AtomicU64,
    pub egress_denied_total: AtomicU64,
    pub fd_denied_total: AtomicU64,
    pub fs_write_denied_total: AtomicU64,
    pub bind_denied_total: AtomicU64,
    pub dns_denied_total: AtomicU64,
    pub memory_growth_denied_total: AtomicU64,
    pub table_growth_denied_total: AtomicU64,
}

impl Default for PolicyCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyCounters {
    pub fn new() -> Self {
        PolicyCounters {
            outbound_connections_active: AtomicU32::new(0),
            outbound_connections_total: AtomicU64::new(0),
            egress_bytes: AtomicU64::new(0),
            dns_lookups_total: AtomicU64::new(0),
            inbound_connections_active: AtomicU32::new(0),
            current_memory_bytes: AtomicU64::new(0),
            memory_bytes_peak: AtomicU64::new(0),
            current_table_elements: AtomicU32::new(0),
            table_elements_peak: AtomicU32::new(0),
            open_fds: AtomicU32::new(0),
            open_fds_peak: AtomicU32::new(0),
            fd_open_total: AtomicU64::new(0),
            fs_write_bytes: AtomicU64::new(0),
            fs_read_bytes: AtomicU64::new(0),
            file_creates_total: AtomicU64::new(0),
            file_deletes_total: AtomicU64::new(0),
            connection_denied_total: AtomicU64::new(0),
            egress_denied_total: AtomicU64::new(0),
            fd_denied_total: AtomicU64::new(0),
            fs_write_denied_total: AtomicU64::new(0),
            bind_denied_total: AtomicU64::new(0),
            dns_denied_total: AtomicU64::new(0),
            memory_growth_denied_total: AtomicU64::new(0),
            table_growth_denied_total: AtomicU64::new(0),
        }
    }
}
