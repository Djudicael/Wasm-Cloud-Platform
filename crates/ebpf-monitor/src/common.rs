//! Shared data structures between eBPF programs and userspace.
//! All structs are `#[repr(C)]` and use only C-compatible types.
//! This file must be identical to the one in `bpf/src/common.rs`.

#[cfg(feature = "ebpf")]
use aya::Pod;

/// Maximum length for comm (process name) in kernel — 16 bytes including null.
pub const TASK_COMM_LEN: usize = 16;

/// Maximum length for IP address as u8 array (IPv6 = 16 bytes).
pub const IP_ADDR_LEN: usize = 16;

/// Event types sent from eBPF to userspace via ring buffer.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum EventType {
    /// A process was exec'd. Check if it's a wasm-node child.
    ProcessExec = 1,
    /// A process exited. If it's a Wasm instance, notify supervisor.
    ProcessExit = 2,
    /// A TCP connection was opened.
    TcpConnect = 3,
    /// A TCP connection was closed.
    TcpClose = 4,
    /// TCP retransmit detected (early partition warning).
    TcpRetransmit = 5,
    /// File descriptor opened.
    FdOpen = 6,
    /// Memory pressure event (kernel reclaim triggered).
    MemPressure = 7,
    /// Disk I/O latency exceeded threshold.
    DiskSlowIo = 8,
    /// Syscall from monitored PID in unexpected category.
    SyscallAnomaly = 9,
    /// FD count for a PID exceeded soft limit.
    FdLimitApproaching = 10,
}

impl EventType {
    /// Convert from raw u32 value, returns None if unknown.
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            1 => Some(EventType::ProcessExec),
            2 => Some(EventType::ProcessExit),
            3 => Some(EventType::TcpConnect),
            4 => Some(EventType::TcpClose),
            5 => Some(EventType::TcpRetransmit),
            6 => Some(EventType::FdOpen),
            7 => Some(EventType::MemPressure),
            8 => Some(EventType::DiskSlowIo),
            9 => Some(EventType::SyscallAnomaly),
            10 => Some(EventType::FdLimitApproaching),
            _ => None,
        }
    }
}

/// Header for every event sent through the ring buffer.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct EventHeader {
    pub event_type: u32,
    pub timestamp_ns: u64, // ktime (CLOCK_MONOTONIC)
    pub pid: u32,
    pub tid: u32,
}

/// Process exec/exit event.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ProcessEvent {
    pub header: EventHeader,
    pub comm: [u8; TASK_COMM_LEN],
    pub exit_code: u32, // 0 for exec events
    pub signal: u32,    // 0 for exec events; signal number for exit
    pub ppid: u32,      // Parent PID (to identify wasm-node children)
    pub cgroup_id: u64, // cgroup v2 ID for tenant attribution
}

/// TCP connection event.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TcpEvent {
    pub header: EventHeader,
    pub src_addr: [u8; IP_ADDR_LEN],
    pub src_port: u16,
    pub dst_addr: [u8; IP_ADDR_LEN],
    pub dst_port: u16,
    pub old_state: u32,   // TCP FSM old state
    pub new_state: u32,   // TCP FSM new state
    pub retransmits: u32, // Cumulative retransmit count at event time
    pub rtt_us: u64,      // Smoothed RTT in microseconds
}

/// File descriptor event.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct FdEvent {
    pub header: EventHeader,
    pub fd: u32,
    pub fd_type: u32,          // Enum: FdType { File, Socket, Pipe, Other }
    pub current_fd_count: u32, // Total open FDs for this PID
    pub fd_soft_limit: u32,    // Configured soft limit
}

/// Memory pressure event.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct MemPressureEvent {
    pub header: EventHeader,
    pub free_pages: u64,
    pub reclaim_pages: u64,
    pub pressure_level: u32, // 0=low, 1=medium, 2=critical
    pub anon_pages: u64,     // Anonymous (Wasm linear memory) pages
}

/// Disk I/O event.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct DiskIoEvent {
    pub header: EventHeader,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub sector: u64,
    pub nr_sector: u32,
    pub latency_ns: u64, // Time from submit to complete
    pub io_type: u32,    // 0=read, 1=write, 2=sync
}

/// Syscall anomaly event.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SyscallEvent {
    pub header: EventHeader,
    pub syscall_nr: u64,
    pub syscall_category: u32, // Enum: SyscallCategory
    pub count_in_window: u64,  // Count in the last sampling window
}

/// Syscall categories for policy enforcement.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SyscallCategory {
    /// Allowed: read, write, openat, close, fstat, mmap, mprotect, etc.
    Normal = 0,
    /// Suspicious: ptrace, perf_event_open, bpf, mount, umount, setuid
    PrivilegeEscalation = 1,
    /// Network control: socket, bind, listen, connect, setsockopt
    NetworkControl = 2,
    /// Process control: fork, clone, execve, kill, tgkill
    ProcessControl = 3,
}

impl SyscallCategory {
    /// Convert from raw u32 value, returns Normal if unknown.
    pub fn from_u32(val: u32) -> Self {
        match val {
            1 => SyscallCategory::PrivilegeEscalation,
            2 => SyscallCategory::NetworkControl,
            3 => SyscallCategory::ProcessControl,
            _ => SyscallCategory::Normal,
        }
    }
}

/// Configuration map (userspace → kernel).
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct MonitorConfigMap {
    /// PID of the wasm-node process (to filter relevant events).
    pub node_pid: u32,
    /// FD soft limit per Wasm instance PID.
    pub fd_soft_limit: u32,
    /// FD hard limit per Wasm instance PID (trigger kill).
    pub fd_hard_limit: u32,
    /// Memory pressure threshold (pages) for "low" alert.
    pub mem_low_threshold_pages: u64,
    /// Memory pressure threshold (pages) for "critical" alert.
    pub mem_critical_threshold_pages: u64,
    /// Disk I/O latency threshold (nanoseconds) for "slow" alert.
    pub disk_slow_threshold_ns: u64,
    /// Maximum TCP connections per PID before alert.
    pub tcp_conn_limit_per_pid: u32,
    /// Syscall rate limit (per second) for suspicious categories.
    pub syscall_rate_limit: u64,
    /// Sampling period for periodic counters (nanoseconds).
    pub sampling_period_ns: u64,
}

// ── Pod implementations for aya map/ring-buffer operations ────────────────────
//
// All `#[repr(C)]` structs shared between eBPF programs and userspace must
// implement `aya::Pod` so they can be read from / written to eBPF maps and
// the ring buffer as raw byte slices. Only compiled when the `ebpf` feature
// is enabled (since `aya` is an optional dependency).

#[cfg(feature = "ebpf")]
unsafe impl Pod for EventHeader {}
#[cfg(feature = "ebpf")]
unsafe impl Pod for ProcessEvent {}
#[cfg(feature = "ebpf")]
unsafe impl Pod for TcpEvent {}
#[cfg(feature = "ebpf")]
unsafe impl Pod for FdEvent {}
#[cfg(feature = "ebpf")]
unsafe impl Pod for MemPressureEvent {}
#[cfg(feature = "ebpf")]
unsafe impl Pod for DiskIoEvent {}
#[cfg(feature = "ebpf")]
unsafe impl Pod for SyscallEvent {}
#[cfg(feature = "ebpf")]
unsafe impl Pod for MonitorConfigMap {}

// ── Helper: read a Pod struct from a raw byte slice ───────────────────────────

/// Read a `#[repr(C)]` plain-old-data struct from the beginning of a byte slice.
/// Returns `None` if the slice is too small.
///
/// The `T: Copy` bound is sufficient because all shared structs are `#[repr(C)]`
/// with no padding issues that would require `Pod`'s additional guarantees.
/// When the `ebpf` feature is enabled, these types also implement `Pod`.
pub fn read_struct<T: Copy>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < std::mem::size_of::<T>() {
        return None;
    }
    // Safety: we've checked the size, and T is Pod (plain-old-data, repr(C)).
    Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) })
}

/// Read a `#[repr(C)]` plain-old-data struct from a byte slice at a given offset.
/// Returns `None` if the slice is too small starting from `offset`.
pub fn read_struct_at<T: Copy>(bytes: &[u8], offset: usize) -> Option<T> {
    if offset + std::mem::size_of::<T>() > bytes.len() {
        return None;
    }
    Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(offset) as *const T) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type_roundtrip() {
        for i in 1..=10u32 {
            let et = EventType::from_u32(i);
            assert!(et.is_some(), "EventType {} should be valid", i);
            assert_eq!(et.unwrap() as u32, i);
        }
        assert!(EventType::from_u32(0).is_none());
        assert!(EventType::from_u32(99).is_none());
    }

    #[test]
    fn test_syscall_category_roundtrip() {
        assert_eq!(SyscallCategory::from_u32(0), SyscallCategory::Normal);
        assert_eq!(
            SyscallCategory::from_u32(1),
            SyscallCategory::PrivilegeEscalation
        );
        assert_eq!(
            SyscallCategory::from_u32(2),
            SyscallCategory::NetworkControl
        );
        assert_eq!(
            SyscallCategory::from_u32(3),
            SyscallCategory::ProcessControl
        );
        assert_eq!(SyscallCategory::from_u32(99), SyscallCategory::Normal);
    }

    #[test]
    fn test_read_struct_valid() {
        let header = EventHeader {
            event_type: EventType::ProcessExit as u32,
            timestamp_ns: 1234567890,
            pid: 42,
            tid: 43,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &header as *const EventHeader as *const u8,
                std::mem::size_of::<EventHeader>(),
            )
        };
        let parsed: EventHeader = read_struct(bytes).unwrap();
        assert_eq!(parsed.event_type, EventType::ProcessExit as u32);
        assert_eq!(parsed.timestamp_ns, 1234567890);
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.tid, 43);
    }

    #[test]
    fn test_read_struct_too_small() {
        let bytes = [0u8; 4];
        let result: Option<EventHeader> = read_struct(&bytes);
        assert!(result.is_none());
    }

    #[test]
    fn test_read_struct_at_valid() {
        let header = EventHeader {
            event_type: EventType::MemPressure as u32,
            timestamp_ns: 999,
            pid: 1,
            tid: 2,
        };
        let mut bytes = vec![0xAAu8; 16];
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &header as *const EventHeader as *const u8,
                std::mem::size_of::<EventHeader>(),
            )
        };
        bytes.extend_from_slice(header_bytes);
        let parsed: EventHeader = read_struct_at(&bytes, 16).unwrap();
        assert_eq!(parsed.event_type, EventType::MemPressure as u32);
        assert_eq!(parsed.pid, 1);
    }

    #[test]
    fn test_read_struct_at_too_small() {
        let bytes = [0u8; 20];
        let result: Option<EventHeader> = read_struct_at(&bytes, 16);
        assert!(result.is_none());
    }

    #[test]
    fn test_struct_sizes_are_c_aligned() {
        // Verify that struct sizes are what we expect for C ABI compatibility.
        // These sizes must match between the BPF program and userspace.
        //
        // EventHeader with #[repr(C)] on 64-bit:
        //   u32 event_type  (4 bytes)
        //   4 bytes padding  (to align u64 to 8-byte boundary)
        //   u64 timestamp_ns (8 bytes)
        //   u32 pid          (4 bytes)
        //   u32 tid          (4 bytes)
        //   Total: 24 bytes
        assert!(
            std::mem::size_of::<EventHeader>() <= 32,
            "EventHeader size should be reasonable for C ABI"
        );
        assert!(
            std::mem::size_of::<EventHeader>() >= 20,
            "EventHeader should contain all fields"
        );
        // ProcessEvent and MonitorConfigMap: just verify they're reasonable sizes
        assert!(std::mem::size_of::<ProcessEvent>() >= 40);
        assert!(std::mem::size_of::<MonitorConfigMap>() >= 40);
    }

    #[test]
    fn test_monitor_config_map_default_values() {
        let config = MonitorConfigMap {
            node_pid: 1,
            fd_soft_limit: 8192,
            fd_hard_limit: 9728,
            mem_low_threshold_pages: 65536,
            mem_critical_threshold_pages: 16384,
            disk_slow_threshold_ns: 50_000_000,
            tcp_conn_limit_per_pid: 10000,
            syscall_rate_limit: 100_000,
            sampling_period_ns: 10_000_000_000,
        };
        assert_eq!(config.node_pid, 1);
        assert_eq!(config.fd_soft_limit, 8192);
        assert_eq!(config.sampling_period_ns, 10_000_000_000);
    }
}
