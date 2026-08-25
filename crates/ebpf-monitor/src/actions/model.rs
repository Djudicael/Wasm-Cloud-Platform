use crate::common::{EventType, SyscallCategory};

/// Actions that the eBPF monitor can trigger.
#[derive(Debug, Clone)]
pub enum RecoveryAction {
    RemoveFromUpstream {
        pid: u32,
    },
    KillInstance {
        pid: u32,
        reason: String,
    },
    ActivateBackpressure {
        reason: String,
    },
    DeactivateBackpressure,
    EnterDegradedMode {
        reason: String,
    },
    ExitDegradedMode,
    PruneIdleInstances,
    SecurityIncident {
        pid: u32,
        syscall_nr: u64,
        category: String,
    },
    WarnOnly {
        message: String,
    },
    NamespaceSecurityIncident {
        tid: u32,
        namespace: String,
        app_id: String,
        incident_type: NamespaceIncidentType,
    },
}

#[derive(Debug, Clone)]
pub enum NamespaceIncidentType {
    ForgedHeader,
    UnregisteredTidAccess,
}

/// Events read from the eBPF ring buffer or produced by the userspace fallback.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    ProcessExec {
        pid: u32,
        ppid: u32,
        comm: [u8; 16],
        cgroup_id: u64,
    },
    ProcessExit {
        pid: u32,
        ppid: u32,
        exit_code: u32,
        signal: u32,
        comm: [u8; 16],
        cgroup_id: u64,
    },
    TcpConnect {
        pid: u32,
        src_port: u16,
        dst_port: u16,
        old_state: u32,
        new_state: u32,
    },
    TcpClose {
        pid: u32,
        src_port: u16,
        dst_port: u16,
    },
    TcpRetransmit {
        pid: u32,
        src_port: u16,
        dst_port: u16,
        retransmits: u32,
        rtt_us: u64,
    },
    FdOpen {
        pid: u32,
        fd: u32,
        current_fd_count: u32,
        fd_soft_limit: u32,
    },
    FdLimitApproaching {
        pid: u32,
        fd: u32,
        current_fd_count: u32,
        fd_soft_limit: u32,
    },
    MemPressure {
        pid: u32,
        free_pages: u64,
        reclaim_pages: u64,
        pressure_level: u32,
        anon_pages: u64,
    },
    DiskSlowIo {
        dev_major: u32,
        dev_minor: u32,
        latency_ns: u64,
        io_type: u32,
    },
    SyscallAnomaly {
        pid: u32,
        tid: u32,
        syscall_nr: u64,
        syscall_category: SyscallCategory,
        count_in_window: u64,
    },
    TidConnection {
        tid: u32,
        namespace: String,
        app_id: String,
        source_port: u16,
    },
    TidDisconnection {
        tid: u32,
        source_port: u16,
    },
    NamespaceAudit {
        tid: u32,
        namespace: String,
        app_id: String,
    },
    NamespaceForgedHeader {
        tid: u32,
        namespace: String,
        app_id: String,
    },
    UnregisteredTidConnection {
        tid: u32,
    },
}

impl MonitorEvent {
    pub fn event_type(&self) -> EventType {
        match self {
            MonitorEvent::ProcessExec { .. } => EventType::ProcessExec,
            MonitorEvent::ProcessExit { .. } => EventType::ProcessExit,
            MonitorEvent::TcpConnect { .. } => EventType::TcpConnect,
            MonitorEvent::TcpClose { .. } => EventType::TcpClose,
            MonitorEvent::TcpRetransmit { .. } => EventType::TcpRetransmit,
            MonitorEvent::FdOpen { .. } => EventType::FdOpen,
            MonitorEvent::FdLimitApproaching { .. } => EventType::FdLimitApproaching,
            MonitorEvent::MemPressure { .. } => EventType::MemPressure,
            MonitorEvent::DiskSlowIo { .. } => EventType::DiskSlowIo,
            MonitorEvent::SyscallAnomaly { .. } => EventType::SyscallAnomaly,
            MonitorEvent::TidConnection { .. } => EventType::TidConnection,
            MonitorEvent::TidDisconnection { .. } => EventType::TidDisconnection,
            MonitorEvent::NamespaceAudit { .. } => EventType::NamespaceAudit,
            MonitorEvent::NamespaceForgedHeader { .. } => EventType::NamespaceForgedHeader,
            MonitorEvent::UnregisteredTidConnection { .. } => EventType::TidConnection,
        }
    }
}
