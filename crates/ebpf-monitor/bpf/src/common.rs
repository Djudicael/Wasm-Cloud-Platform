//! Shared data structures between eBPF programs and userspace.
//! All structs are `#[repr(C)]` and use only C-compatible types.

/// Maximum length for comm (process name) in kernel — 16 bytes including null.
pub const TASK_COMM_LEN: usize = 16;

/// Maximum length for IP address as u8 array (IPv6 = 16 bytes).
pub const IP_ADDR_LEN: usize = 16;

/// Event types sent from eBPF to userspace via ring buffer.
#[repr(u32)]
#[derive(Copy, Clone)]
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

    // ── Namespace enforcement events ──
    /// A monitored TID established a TCP connection to the gateway.
    TidConnection = 11,
    /// A monitored TID closed a TCP connection to the gateway.
    TidDisconnection = 12,
    /// Namespace audit event (gateway request, connection, etc.).
    NamespaceAudit = 13,
    /// A forged namespace header was detected in send buffer.
    NamespaceForgedHeader = 14,
}

/// Header for every event sent through the ring buffer.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct EventHeader {
    pub event_type: u32,
    pub _padding: u32,
    pub timestamp_ns: u64, // ktime (CLOCK_MONOTONIC)
    pub pid: u32,
    pub tid: u32,
}

/// Process exec/exit event.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ProcessEvent {
    pub header: EventHeader,
    pub comm: [u8; TASK_COMM_LEN],
    pub exit_code: u32, // 0 for exec events
    pub signal: u32,    // 0 for exec events; signal number for exit
    pub ppid: u32,      // Parent PID (to identify wasm-node children)
    pub _padding: u32,
    pub cgroup_id: u64, // cgroup v2 ID for tenant attribution
}

/// TCP connection event.
#[repr(C)]
#[derive(Copy, Clone)]
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
#[derive(Copy, Clone)]
pub struct FdEvent {
    pub header: EventHeader,
    pub fd: u32,
    pub fd_type: u32,          // Enum: FdType { File, Socket, Pipe, Other }
    pub current_fd_count: u32, // Total open FDs for this PID
    pub fd_soft_limit: u32,    // Configured soft limit
}

/// Memory pressure event.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct MemPressureEvent {
    pub header: EventHeader,
    pub free_pages: u64,
    pub reclaim_pages: u64,
    pub pressure_level: u32, // 0=low, 1=medium, 2=critical
    pub _padding: u32,
    pub anon_pages: u64,     // Anonymous (Wasm linear memory) pages
}

/// Disk I/O event.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct DiskIoEvent {
    pub header: EventHeader,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub sector: u64,
    pub nr_sector: u32,
    pub _padding1: u32,
    pub latency_ns: u64, // Time from submit to complete
    pub io_type: u32,    // 0=read, 1=write, 2=sync
    pub _padding2: u32,
}

/// Syscall anomaly event.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SyscallEvent {
    pub header: EventHeader,
    pub syscall_nr: u64,
    pub syscall_category: u32, // Enum: SyscallCategory
    pub _padding: u32,
    pub count_in_window: u64,  // Count in the last sampling window
}

/// Syscall categories for policy enforcement.
#[repr(u32)]
#[derive(Copy, Clone)]
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

/// Configuration map (userspace → kernel).
#[repr(C)]
#[derive(Copy, Clone)]
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

// ── Namespace Enforcement Types ───────────────────────────────────────────────

/// Identity stored per TID in the MONITORED_TIDS eBPF map.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct TidIdentity {
    /// Namespace name (null-terminated UTF-8, max 63 chars + null).
    pub namespace: [u8; 64],
    /// App ID (null-terminated UTF-8, max 63 chars + null).
    pub app_id: [u8; 64],
    /// Monotonic timestamp when this TID was registered.
    pub registered_at_ns: u64,
    /// Flags — see `TidFlags`.
    pub flags: u32,
    /// Padding to ensure 8-byte alignment.
    pub _padding: u32,
}

/// Flags for TidIdentity.
#[repr(u32)]
#[derive(Copy, Clone)]
pub enum TidFlags {
    /// TID is actively monitored.
    Enabled = 1,
    /// Only audit, do not enforce (for canary/testing).
    AuditOnly = 2,
}

/// Namespace enforcement configuration (singleton in NS_ENFORCE_CONFIG map).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct NsEnforceConfig {
    /// Port the internal gateway listens on (usually 9080).
    pub gateway_port: u16,
    /// Padding to align flags to 4 bytes.
    pub _padding1: u16,
    /// Enforcement flags — see `NsEnforceFlags`.
    pub flags: u32,
    /// PID of the wasm-node process (to filter relevant TIDs).
    pub node_pid: u32,
    /// Reserved for future use.
    pub _reserved: u32,
}

/// Flags for NsEnforceConfig.
#[repr(u32)]
#[derive(Copy, Clone)]
pub enum NsEnforceFlags {
    /// Enable audit logging.
    EnableAudit = 1,
    /// Enable forged header detection.
    EnableForgedHeaderDetect = 2,
    /// Enable SK_MSG enforcement (Linux 5.8+).
    EnableSkMsg = 4,
}

/// Audit event emitted by eBPF for namespace enforcement.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct NamespaceAuditEvent {
    /// Event header (type, timestamp, pid, tid).
    pub header: EventHeader,
    /// Audit type — see `NamespaceAuditType`.
    pub audit_type: u32,
    /// Source namespace (null-terminated).
    pub source_namespace: [u8; 64],
    /// Source app ID (null-terminated).
    pub source_app_id: [u8; 64],
    /// Destination port (gateway port).
    pub dest_port: u16,
    /// Source port of the TCP connection.
    pub source_port: u16,
    /// Padding.
    pub _padding: u32,
    /// Tail padding required by the header's 8-byte alignment.
    pub _tail_padding: u32,
}

/// Types of namespace audit events.
#[repr(u32)]
#[derive(Copy, Clone)]
pub enum NamespaceAuditType {
    /// A request arrived at the gateway from this TID.
    GatewayRequest = 1,
    /// TCP connection established to gateway.
    ConnectionEstablished = 2,
    /// TCP connection to gateway closed.
    ConnectionClosed = 3,
    /// Forged X-Namespace header detected in send buffer.
    ForgedHeader = 4,
    /// Unregistered TID connected to gateway.
    UnregisteredTid = 5,
}
