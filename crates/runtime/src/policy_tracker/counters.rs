use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Default)]
struct HostManagedTcpCloses {
    enabled: bool,
    pending: usize,
    ports: HashMap<(u16, u16), u32>,
}

// Bound retained close-event ownership even when the kernel consumer stalls.
const MAX_PENDING_HOST_TCP_CLOSES: usize = 65_536;

/// Atomic counters for a single instance's policy enforcement.
/// Shared between the WASI host functions and the metrics exporter.
#[derive(Debug)]
pub struct PolicyCounters {
    host_socket_lifecycle: AtomicBool,
    host_tcp_closes: Mutex<HostManagedTcpCloses>,
    // Network counters
    pub outbound_connections_active: AtomicU32,
    pub outbound_connections_total: AtomicU64,
    pub egress_bytes: AtomicU64,
    pub io_connect_timeouts: AtomicU64,
    pub io_read_timeouts: AtomicU64,
    pub io_write_timeouts: AtomicU64,
    pub io_http_timeouts: AtomicU64,
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
            host_socket_lifecycle: AtomicBool::new(false),
            host_tcp_closes: Mutex::new(HostManagedTcpCloses::default()),
            outbound_connections_active: AtomicU32::new(0),
            outbound_connections_total: AtomicU64::new(0),
            egress_bytes: AtomicU64::new(0),
            io_connect_timeouts: AtomicU64::new(0),
            io_read_timeouts: AtomicU64::new(0),
            io_write_timeouts: AtomicU64::new(0),
            io_http_timeouts: AtomicU64::new(0),
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

    pub fn enable_host_socket_lifecycle(&self) {
        self.host_socket_lifecycle.store(true, Ordering::Release);
    }

    pub fn uses_host_socket_lifecycle(&self) -> bool {
        self.host_socket_lifecycle.load(Ordering::Acquire)
    }

    /// Enable before guest execution when a supervisor consumes kernel closes.
    pub fn enable_host_tcp_close_tracking(&self) {
        self.host_tcp_closes
            .lock()
            .expect("TCP close ownership lock")
            .enabled = true;
    }

    /// Legacy raw consumers defer teardown to this observer; platform stores
    /// enable host resource-lifecycle accounting instead.
    pub(crate) fn tcp_close_observation_enabled(&self) -> bool {
        self.host_tcp_closes
            .lock()
            .expect("TCP close ownership lock")
            .enabled
    }

    /// Register before connecting a host-managed socket. Keep the token until
    /// its kernel close event, even after RAII teardown, so delayed events cannot
    /// release another connection's slot. A stalled observer fails closed at a
    /// bounded backlog; no observer requires no retained ownership tokens.
    pub fn track_host_tcp_close(&self, source_port: u16, destination_port: u16) -> bool {
        if self.uses_host_socket_lifecycle() {
            return true;
        }
        let mut state = self
            .host_tcp_closes
            .lock()
            .expect("TCP close ownership lock");
        if !state.enabled {
            return true;
        }
        if state.pending >= MAX_PENDING_HOST_TCP_CLOSES {
            return false;
        }
        *state
            .ports
            .entry((source_port, destination_port))
            .or_default() += 1;
        state.pending += 1;
        true
    }

    /// True means RAII owns this close; kernel accounting must not decrement.
    pub fn consume_host_tcp_close(&self, source_port: u16, destination_port: u16) -> bool {
        let mut state = self
            .host_tcp_closes
            .lock()
            .expect("TCP close ownership lock");
        let key = (source_port, destination_port);
        let Some(count) = state.ports.get_mut(&key) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            state.ports.remove(&key);
        }
        state.pending -= 1;
        true
    }
}

#[cfg(test)]
mod close_tracking_tests {
    use super::*;

    #[test]
    fn stalled_close_observation_is_bounded_and_reused_ports_keep_distinct_tokens() {
        let counters = PolicyCounters::new();
        counters.enable_host_tcp_close_tracking();
        for _ in 0..MAX_PENDING_HOST_TCP_CLOSES {
            assert!(counters.track_host_tcp_close(40_000, 443));
        }
        assert!(!counters.track_host_tcp_close(40_001, 443));
        assert!(counters.consume_host_tcp_close(40_000, 443));
        assert!(counters.track_host_tcp_close(40_001, 443));
        assert!(counters.consume_host_tcp_close(40_001, 443));
        assert!(!counters.consume_host_tcp_close(40_001, 443));
    }
}
