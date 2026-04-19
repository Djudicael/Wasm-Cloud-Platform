//! Recovery action executor for eBPF monitor events.
//!
//! The `ActionDispatcher` receives parsed `MonitorEvent`s from the ring buffer
//! consumer (or the userspace fallback) and determines the appropriate recovery
//! action. It updates Prometheus metrics for every event and invokes platform
//! callbacks (backpressure, NATS health, event publishing) as needed.
//!
//! # Design: Callback Trait
//!
//! The dispatcher does **not** depend directly on `proxy` or `messaging` crates
//! to avoid circular dependencies. Instead, it accepts a `dyn EventCallbacks`
//! trait object that the node's `main.rs` implements with concrete types.

use crate::MonitorConfig;
use std::sync::Arc;

use tracing::{error, info, warn};

use crate::common::{EventType, SyscallCategory};
use crate::metrics::EbpfMetrics;

// ── Recovery Actions ──────────────────────────────────────────────────────────

/// Actions that the eBPF monitor can trigger.
/// These are determined by the `ActionDispatcher` based on incoming events
/// and dispatched to the platform via the `EventCallbacks` trait.
#[derive(Debug, Clone)]
pub enum RecoveryAction {
    /// Remove a dead instance from the upstream table immediately.
    RemoveFromUpstream { pid: u32 },

    /// Kill an instance that is consuming too many resources.
    KillInstance { pid: u32, reason: String },

    /// Activate backpressure (stop accepting new connections).
    ActivateBackpressure { reason: String },

    /// Deactivate backpressure (resume accepting connections).
    DeactivateBackpressure,

    /// Enter degraded mode (e.g., NATS partition likely, disk I/O slow).
    EnterDegradedMode { reason: String },

    /// Exit degraded mode (recovered from pressure).
    ExitDegradedMode,

    /// Prune all idle instances to free memory/FDs.
    PruneIdleInstances,

    /// Security incident — quarantine an artifact and kill the instance.
    SecurityIncident {
        pid: u32,
        syscall_nr: u64,
        category: String,
    },

    /// Log a warning (no automated action, but emit metric).
    WarnOnly { message: String },
}

// ── Monitor Events ────────────────────────────────────────────────────────────

/// Events read from the eBPF ring buffer (or produced by the userspace fallback),
/// parsed and ready for dispatch.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    /// A process was exec'd (child of wasm-node).
    ProcessExec {
        pid: u32,
        ppid: u32,
        comm: [u8; 16],
        cgroup_id: u64,
    },

    /// A process exited. If it's a Wasm instance, notify supervisor.
    ProcessExit {
        pid: u32,
        ppid: u32,
        exit_code: u32,
        signal: u32,
        comm: [u8; 16],
        cgroup_id: u64,
    },

    /// A TCP connection was opened.
    TcpConnect {
        pid: u32,
        src_port: u16,
        dst_port: u16,
        old_state: u32,
        new_state: u32,
    },

    /// A TCP connection was closed.
    TcpClose {
        pid: u32,
        src_port: u16,
        dst_port: u16,
    },

    /// TCP retransmit detected (early partition warning).
    TcpRetransmit {
        pid: u32,
        src_port: u16,
        dst_port: u16,
        retransmits: u32,
        rtt_us: u64,
    },

    /// File descriptor opened.
    FdOpen {
        pid: u32,
        fd: u32,
        current_fd_count: u32,
        fd_soft_limit: u32,
    },

    /// FD count for a PID exceeded soft limit.
    FdLimitApproaching {
        pid: u32,
        fd: u32,
        current_fd_count: u32,
        fd_soft_limit: u32,
    },

    /// Memory pressure event (kernel reclaim triggered).
    MemPressure {
        pid: u32,
        free_pages: u64,
        reclaim_pages: u64,
        pressure_level: u32,
        anon_pages: u64,
    },

    /// Disk I/O latency exceeded threshold.
    DiskSlowIo {
        dev_major: u32,
        dev_minor: u32,
        latency_ns: u64,
        io_type: u32,
    },

    /// Syscall from monitored PID in unexpected category.
    SyscallAnomaly {
        pid: u32,
        syscall_nr: u64,
        syscall_category: SyscallCategory,
        count_in_window: u64,
    },
}

impl MonitorEvent {
    /// Return the `EventType` discriminant for this event.
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
        }
    }
}

// ── Callback Trait ────────────────────────────────────────────────────────────

/// Platform callbacks invoked by the `ActionDispatcher` when recovery actions
/// are needed. This trait decouples the eBPF monitor from the proxy, messaging,
/// and supervisor crates, avoiding circular dependencies.
///
/// The node's `main.rs` provides a concrete implementation that wraps the actual
/// platform types (`BackpressureSignal`, `NatsHealth`, `NatsBus`, etc.).
pub trait EventCallbacks: Send + Sync {
    /// Activate backpressure — stop accepting new connections.
    fn activate_backpressure(&self, reason: &str);

    /// Deactivate backpressure — resume accepting connections.
    fn deactivate_backpressure(&self);

    /// Mark the NATS connection as disconnected (pre-emptive, before actual disconnect).
    fn mark_nats_disconnected(&self);

    /// Publish a `NodeUnderPressure` event to the NATS control plane.
    fn publish_node_under_pressure(&self, node_id: &str, pressure_level: u32);

    /// Publish a `NodePressureRecovered` event to the NATS control plane.
    fn publish_node_pressure_recovered(&self, node_id: &str);

    /// Publish a `SecurityIncident` event to the NATS control plane.
    fn publish_security_incident(&self, node_id: &str, pid: u32, syscall_nr: u64, category: &str);

    /// Request the supervisor to kill a specific instance by PID.
    fn kill_instance(&self, pid: u32, reason: &str);

    /// Request the supervisor to prune all idle instances.
    fn prune_idle_instances(&self);

    /// Remove a dead instance from the upstream routing table.
    fn remove_from_upstream(&self, pid: u32);
}

/// A no-op implementation of `EventCallbacks` for testing and as a safe default.
pub struct NoopCallbacks;

impl EventCallbacks for NoopCallbacks {
    fn activate_backpressure(&self, _reason: &str) {}
    fn deactivate_backpressure(&self) {}
    fn mark_nats_disconnected(&self) {}
    fn publish_node_under_pressure(&self, _node_id: &str, _pressure_level: u32) {}
    fn publish_node_pressure_recovered(&self, _node_id: &str) {}
    fn publish_security_incident(
        &self,
        _node_id: &str,
        _pid: u32,
        _syscall_nr: u64,
        _category: &str,
    ) {
    }
    fn kill_instance(&self, _pid: u32, _reason: &str) {}
    fn prune_idle_instances(&self) {}
    fn remove_from_upstream(&self, _pid: u32) {}
}

// ── Action Dispatcher ─────────────────────────────────────────────────────────

/// Process monitor events and determine recovery actions.
///
/// The dispatcher:
/// 1. Updates Prometheus metrics for every event
/// 2. Determines the appropriate recovery action based on event type and thresholds
/// 3. Invokes platform callbacks to execute the action
///
/// # Graduated Response
///
/// Memory pressure and FD exhaustion use a graduated response:
/// - **Low**: Log + metrics only
/// - **Medium**: Backpressure + instance pruning
/// - **Critical**: Instance killing + cluster-wide pressure event
pub struct ActionDispatcher {
    /// Prometheus metrics (always updated, regardless of callbacks).
    pub metrics: Arc<EbpfMetrics>,
    /// Platform callbacks for recovery actions.
    callbacks: Arc<dyn EventCallbacks>,
    /// Node ID for publishing cluster events.
    node_id: String,
    /// Whether backpressure is currently active (to avoid duplicate signals).
    backpressure_active: std::sync::atomic::AtomicBool,
    /// Whether the node is in degraded mode (to avoid duplicate signals).
    degraded_mode: std::sync::atomic::AtomicBool,
    /// Last memory pressure level (to detect recovery).
    last_pressure_level: std::sync::atomic::AtomicU32,
    /// Current monitor configuration (hot-reloadable thresholds).
    /// Updated via [`update_thresholds`] when the operator changes eBPF
    /// parameters through the admin API.
    config: std::sync::RwLock<MonitorConfig>,
}

impl ActionDispatcher {
    /// Create a new `ActionDispatcher`.
    pub fn new(
        metrics: Arc<EbpfMetrics>,
        callbacks: Arc<dyn EventCallbacks>,
        node_id: String,
    ) -> Self {
        ActionDispatcher {
            metrics,
            callbacks,
            node_id,
            backpressure_active: std::sync::atomic::AtomicBool::new(false),
            degraded_mode: std::sync::atomic::AtomicBool::new(false),
            last_pressure_level: std::sync::atomic::AtomicU32::new(0),
            config: std::sync::RwLock::new(MonitorConfig::default()),
        }
    }

    /// Create a dispatcher with no-op callbacks (for testing).
    pub fn new_noop(metrics: Arc<EbpfMetrics>, node_id: String) -> Self {
        Self::new(metrics, Arc::new(NoopCallbacks), node_id)
    }

    /// Create a dispatcher with an explicit initial monitor configuration.
    ///
    /// Use this when the eBPF monitor is initialised with a non-default
    /// `MonitorConfig` so that the dispatcher's threshold values match
    /// the actual eBPF program configuration from the start.
    pub fn with_config(
        metrics: Arc<EbpfMetrics>,
        callbacks: Arc<dyn EventCallbacks>,
        node_id: String,
        config: MonitorConfig,
    ) -> Self {
        ActionDispatcher {
            metrics,
            callbacks,
            node_id,
            backpressure_active: std::sync::atomic::AtomicBool::new(false),
            degraded_mode: std::sync::atomic::AtomicBool::new(false),
            last_pressure_level: std::sync::atomic::AtomicU32::new(0),
            config: std::sync::RwLock::new(config),
        }
    }

    /// Update the eBPF monitor thresholds at runtime (hot-reload).
    ///
    /// Called by the config sync loop when the operator changes eBPF
    /// parameters (`fd_soft_limit`, `fd_hard_limit`, `mem_low_threshold_pages`,
    /// etc.) through the admin API. The update takes effect immediately for
    /// subsequent event dispatching decisions.
    ///
    /// Returns the previous config for audit logging.
    pub fn update_thresholds(&self, new_config: MonitorConfig) -> MonitorConfig {
        let mut guard = self.config.write().unwrap();
        let previous = guard.clone();
        tracing::info!(
            old_fd_soft = previous.fd_soft_limit,
            new_fd_soft = new_config.fd_soft_limit,
            new_fd_hard = new_config.fd_hard_limit,
            new_mem_low = new_config.mem_low_threshold_pages,
            new_mem_critical = new_config.mem_critical_threshold_pages,
            new_disk_slow_ns = new_config.disk_slow_threshold_ns,
            new_tcp_limit = new_config.tcp_conn_limit_per_pid,
            new_syscall_rate = new_config.syscall_rate_limit,
            "eBPF monitor thresholds updated via hot-reload"
        );
        *guard = new_config;
        previous
    }

    /// Read the current monitor configuration (for introspection / admin API).
    pub fn current_config(&self) -> MonitorConfig {
        self.config.read().unwrap().clone()
    }

    /// Dispatch a monitor event: update metrics and trigger recovery actions.
    pub fn dispatch(&self, event: MonitorEvent) {
        self.metrics.events_processed.inc();

        match event {
            MonitorEvent::ProcessExec {
                pid,
                ppid,
                comm,
                cgroup_id,
            } => {
                let comm_str = String::from_utf8_lossy(
                    &comm[..comm.iter().position(|&b| b == 0).unwrap_or(comm.len())],
                );
                info!(
                    pid,
                    ppid,
                    comm = %comm_str,
                    cgroup_id,
                    "Process exec detected (child of wasm-node)"
                );
            }

            MonitorEvent::ProcessExit {
                pid,
                ppid,
                exit_code,
                signal,
                comm,
                cgroup_id: _,
            } => {
                let comm_str = String::from_utf8_lossy(
                    &comm[..comm.iter().position(|&b| b == 0).unwrap_or(comm.len())],
                );

                if signal == 9 {
                    // OOM kill — critical
                    error!(
                        pid,
                        ppid,
                        comm = %comm_str,
                        "OOM kill detected for wasm-node child process"
                    );
                    self.metrics.oom_kills.inc();

                    // Immediately remove from upstream table to prevent 502s
                    self.callbacks.remove_from_upstream(pid);

                    // Activate backpressure temporarily
                    self.activate_backpressure_if_needed("OOM kill detected");
                } else if signal != 0 {
                    // Signal death (non-OOM)
                    warn!(
                        pid,
                        ppid,
                        signal,
                        exit_code,
                        comm = %comm_str,
                        "Wasm instance killed by signal"
                    );
                    self.metrics.signal_deaths.inc();

                    // Remove from upstream table preemptively
                    self.callbacks.remove_from_upstream(pid);
                } else {
                    // Normal exit
                    info!(
                        pid,
                        ppid,
                        exit_code,
                        comm = %comm_str,
                        "Wasm instance exited normally"
                    );
                    // Remove from upstream table preemptively (before health loop discovers it)
                    self.callbacks.remove_from_upstream(pid);
                }
                self.metrics.process_exits.inc();
            }

            MonitorEvent::TcpConnect {
                pid,
                src_port,
                dst_port,
                old_state: _,
                new_state: _,
            } => {
                // Informational — connection opened
                tracing::debug!(pid, src_port, dst_port, "TCP connection opened");
                self.metrics.tcp_connection_count.inc();
            }

            MonitorEvent::TcpClose {
                pid: _,
                src_port,
                dst_port,
            } => {
                tracing::debug!(src_port, dst_port, "TCP connection closed");
                self.metrics.tcp_connection_count.dec();
            }

            MonitorEvent::TcpRetransmit {
                pid,
                src_port,
                dst_port,
                retransmits,
                rtt_us,
            } => {
                // If retransmits are on the NATS port, pre-emptively mark disconnected
                if dst_port == 4222 || src_port == 4222 {
                    warn!(
                        pid,
                        src_port,
                        dst_port,
                        retransmits,
                        rtt_us,
                        "NATS TCP retransmits detected — pre-emptive disconnect warning"
                    );
                    self.callbacks.mark_nats_disconnected();
                    self.metrics.nats_retransmit_events.inc();
                } else {
                    warn!(
                        pid,
                        src_port, dst_port, retransmits, rtt_us, "TCP retransmits detected"
                    );
                }
                self.metrics.tcp_retransmits.inc();
            }

            MonitorEvent::FdOpen {
                pid,
                fd,
                current_fd_count,
                fd_soft_limit,
            } => {
                // Informational — FD opened, update metrics
                self.metrics.set_fd_usage(current_fd_count, fd_soft_limit);
                tracing::debug!(pid, fd, current_fd_count, fd_soft_limit, "FD opened");
            }

            MonitorEvent::FdLimitApproaching {
                pid,
                fd,
                current_fd_count,
                fd_soft_limit,
            } => {
                let ratio = current_fd_count as f64 / fd_soft_limit as f64;
                if ratio > 0.95 {
                    // Hard limit approaching — critical
                    error!(
                        pid,
                        fd,
                        current_fd_count,
                        fd_soft_limit,
                        "FD hard limit approaching — pruning idle instances immediately"
                    );
                    self.callbacks.prune_idle_instances();
                    self.activate_backpressure_if_needed("FD hard limit approaching");
                } else {
                    // Soft limit approaching — warning
                    warn!(
                        pid,
                        fd,
                        current_fd_count,
                        fd_soft_limit,
                        "FD soft limit approaching — considering pruning idle instances"
                    );
                    self.callbacks.prune_idle_instances();
                }
                self.metrics.set_fd_usage(current_fd_count, fd_soft_limit);
            }

            MonitorEvent::MemPressure {
                pid: _,
                free_pages,
                reclaim_pages,
                pressure_level,
                anon_pages,
            } => {
                let prev_level = self
                    .last_pressure_level
                    .swap(pressure_level, std::sync::atomic::Ordering::Relaxed);
                self.metrics.set_memory_pressure(pressure_level);

                match pressure_level {
                    0 => {
                        // Low pressure — informational
                        info!(
                            free_pages,
                            reclaim_pages, anon_pages, "Memory pressure: LOW"
                        );
                        // If we were previously under pressure, deactivate backpressure
                        if prev_level >= 2 {
                            self.deactivate_backpressure_if_needed();
                            self.callbacks
                                .publish_node_pressure_recovered(&self.node_id);
                        }
                    }
                    1 => {
                        // Medium pressure — prune idle instances, temporary backpressure
                        warn!(
                            free_pages,
                            reclaim_pages,
                            anon_pages,
                            "Memory pressure: MEDIUM — pruning idle instances"
                        );
                        self.callbacks.prune_idle_instances();
                        self.activate_backpressure_if_needed("Memory pressure: MEDIUM");
                        self.callbacks.publish_node_under_pressure(&self.node_id, 1);
                    }
                    2 => {
                        // Critical pressure — kill largest instance, sustained backpressure
                        error!(
                            free_pages,
                            reclaim_pages,
                            anon_pages,
                            "Memory pressure: CRITICAL — killing largest instance"
                        );
                        self.callbacks.prune_idle_instances();
                        self.activate_backpressure_if_needed("Memory pressure: CRITICAL");
                        self.callbacks.publish_node_under_pressure(&self.node_id, 2);
                        self.enter_degraded_mode_if_needed("Memory pressure: CRITICAL");
                    }
                    _ => {
                        warn!(pressure_level, free_pages, "Unknown memory pressure level");
                    }
                }
            }

            MonitorEvent::DiskSlowIo {
                dev_major,
                dev_minor,
                latency_ns,
                io_type,
            } => {
                let latency_ms = latency_ns as f64 / 1_000_000.0;
                let io_type_str = match io_type {
                    0 => "read",
                    1 => "write",
                    2 => "sync",
                    _ => "unknown",
                };
                warn!(
                    dev = format!("{}:{}", dev_major, dev_minor),
                    latency_ms,
                    io_type = io_type_str,
                    "Slow disk I/O detected"
                );
                self.metrics.observe_disk_latency_ns(latency_ns);

                // If this is the device holding state.redb, enter degraded mode
                // (We can't know the exact device here, so we enter degraded mode
                // for any sustained slow I/O — the node operator can tune the threshold.)
                self.enter_degraded_mode_if_needed(&format!(
                    "Slow disk I/O on {}:{} ({:.1}ms)",
                    dev_major, dev_minor, latency_ms
                ));
            }

            MonitorEvent::SyscallAnomaly {
                pid,
                syscall_nr,
                syscall_category,
                count_in_window,
            } => {
                match syscall_category {
                    SyscallCategory::PrivilegeEscalation => {
                        // Critical security incident — SFI boundary potentially bypassed
                        error!(
                            pid,
                            syscall_nr,
                            count_in_window,
                            "SECURITY: Privilege escalation syscall from Wasm instance!"
                        );
                        self.metrics.security_violations.inc();

                        // Kill the offending instance immediately
                        self.callbacks
                            .kill_instance(pid, "Privilege escalation syscall detected");

                        // Publish security incident to cluster
                        self.callbacks.publish_security_incident(
                            &self.node_id,
                            pid,
                            syscall_nr,
                            "PrivilegeEscalation",
                        );
                    }
                    SyscallCategory::ProcessControl => {
                        // execve from Wasm instance — should never happen
                        error!(
                            pid,
                            syscall_nr,
                            count_in_window,
                            "SECURITY: Process control syscall from Wasm instance!"
                        );
                        self.metrics.security_violations.inc();

                        self.callbacks
                            .kill_instance(pid, "Process control syscall detected");
                        self.callbacks.publish_security_incident(
                            &self.node_id,
                            pid,
                            syscall_nr,
                            "ProcessControl",
                        );
                    }
                    SyscallCategory::NetworkControl => {
                        // Unexpected network syscall — log warning
                        warn!(
                            pid,
                            syscall_nr,
                            count_in_window,
                            "Unexpected network control syscall from Wasm instance"
                        );
                        self.metrics.security_violations.inc();
                    }
                    SyscallCategory::Normal => {
                        // High syscall rate — might be in a tight loop
                        warn!(
                            pid,
                            syscall_nr, count_in_window, "High syscall rate from Wasm instance"
                        );
                    }
                }
            }
        }
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    /// Activate backpressure if not already active.
    fn activate_backpressure_if_needed(&self, reason: &str) {
        if !self
            .backpressure_active
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.callbacks.activate_backpressure(reason);
        }
    }

    /// Deactivate backpressure if currently active.
    fn deactivate_backpressure_if_needed(&self) {
        if self
            .backpressure_active
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.callbacks.deactivate_backpressure();
        }
    }

    /// Enter degraded mode if not already in it.
    fn enter_degraded_mode_if_needed(&self, reason: &str) {
        if !self
            .degraded_mode
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            warn!(reason, "Entering degraded mode");
        }
    }

    /// Exit degraded mode if currently in it.
    pub fn exit_degraded_mode(&self) {
        if self
            .degraded_mode
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            info!("Exiting degraded mode — recovered");
            self.deactivate_backpressure_if_needed();
        }
    }

    /// Check if backpressure is currently active.
    pub fn is_backpressure_active(&self) -> bool {
        self.backpressure_active
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check if the node is in degraded mode.
    pub fn is_degraded(&self) -> bool {
        self.degraded_mode
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get the last memory pressure level.
    pub fn last_pressure_level(&self) -> u32 {
        self.last_pressure_level
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ── Unit Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Registry;
    use std::sync::Mutex;

    /// A test callback implementation that records all calls.
    #[derive(Default)]
    struct TestCallbacks {
        backpressure_activations: Mutex<Vec<String>>,
        backpressure_deactivations: Mutex<usize>,
        nats_disconnected: Mutex<usize>,
        node_under_pressure: Mutex<Vec<(String, u32)>>,
        node_pressure_recovered: Mutex<Vec<String>>,
        security_incidents: Mutex<Vec<(String, u32, u64, String)>>,
        killed_instances: Mutex<Vec<(u32, String)>>,
        pruned: Mutex<usize>,
        removed_from_upstream: Mutex<Vec<u32>>,
    }

    impl EventCallbacks for TestCallbacks {
        fn activate_backpressure(&self, reason: &str) {
            self.backpressure_activations
                .lock()
                .unwrap()
                .push(reason.to_string());
        }
        fn deactivate_backpressure(&self) {
            *self.backpressure_deactivations.lock().unwrap() += 1;
        }
        fn mark_nats_disconnected(&self) {
            *self.nats_disconnected.lock().unwrap() += 1;
        }
        fn publish_node_under_pressure(&self, node_id: &str, pressure_level: u32) {
            self.node_under_pressure
                .lock()
                .unwrap()
                .push((node_id.to_string(), pressure_level));
        }
        fn publish_node_pressure_recovered(&self, node_id: &str) {
            self.node_pressure_recovered
                .lock()
                .unwrap()
                .push(node_id.to_string());
        }
        fn publish_security_incident(
            &self,
            node_id: &str,
            pid: u32,
            syscall_nr: u64,
            category: &str,
        ) {
            self.security_incidents.lock().unwrap().push((
                node_id.to_string(),
                pid,
                syscall_nr,
                category.to_string(),
            ));
        }
        fn kill_instance(&self, pid: u32, reason: &str) {
            self.killed_instances
                .lock()
                .unwrap()
                .push((pid, reason.to_string()));
        }
        fn prune_idle_instances(&self) {
            *self.pruned.lock().unwrap() += 1;
        }
        fn remove_from_upstream(&self, pid: u32) {
            self.removed_from_upstream.lock().unwrap().push(pid);
        }
    }

    fn make_dispatcher(callbacks: Arc<TestCallbacks>) -> ActionDispatcher {
        let registry = Registry::new();
        let metrics = Arc::new(EbpfMetrics::new(&registry));
        ActionDispatcher::new(metrics, callbacks, "test-node".to_string())
    }

    #[test]
    fn test_oom_kill_triggers_backpressure_and_removal() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::ProcessExit {
            pid: 1234,
            ppid: 1,
            exit_code: 0,
            signal: 9, // SIGKILL = OOM
            comm: [0; 16],
            cgroup_id: 0,
        });

        assert!(dispatcher.is_backpressure_active());
        assert_eq!(
            callbacks.removed_from_upstream.lock().unwrap().as_slice(),
            &[1234]
        );
        assert_eq!(dispatcher.metrics.oom_kills.get(), 1);
        assert_eq!(dispatcher.metrics.process_exits.get(), 1);
    }

    #[test]
    fn test_signal_death_removes_from_upstream() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::ProcessExit {
            pid: 5678,
            ppid: 1,
            exit_code: 1,
            signal: 6, // SIGABRT
            comm: [0; 16],
            cgroup_id: 0,
        });

        assert_eq!(
            callbacks.removed_from_upstream.lock().unwrap().as_slice(),
            &[5678]
        );
        assert_eq!(dispatcher.metrics.signal_deaths.get(), 1);
        assert_eq!(dispatcher.metrics.process_exits.get(), 1);
    }

    #[test]
    fn test_normal_exit_removes_from_upstream() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::ProcessExit {
            pid: 9999,
            ppid: 1,
            exit_code: 0,
            signal: 0,
            comm: [0; 16],
            cgroup_id: 0,
        });

        assert_eq!(
            callbacks.removed_from_upstream.lock().unwrap().as_slice(),
            &[9999]
        );
        assert_eq!(dispatcher.metrics.process_exits.get(), 1);
        assert_eq!(dispatcher.metrics.signal_deaths.get(), 0);
        assert_eq!(dispatcher.metrics.oom_kills.get(), 0);
    }

    #[test]
    fn test_nats_retransmit_marks_disconnected() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::TcpRetransmit {
            pid: 1,
            src_port: 4222,
            dst_port: 54321,
            retransmits: 5,
            rtt_us: 1000,
        });

        assert_eq!(*callbacks.nats_disconnected.lock().unwrap(), 1);
        assert_eq!(dispatcher.metrics.nats_retransmit_events.get(), 1);
        assert_eq!(dispatcher.metrics.tcp_retransmits.get(), 1);
    }

    #[test]
    fn test_non_nats_retransmit_does_not_mark_disconnected() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::TcpRetransmit {
            pid: 1,
            src_port: 8080,
            dst_port: 9090,
            retransmits: 3,
            rtt_us: 500,
        });

        assert_eq!(*callbacks.nats_disconnected.lock().unwrap(), 0);
        assert_eq!(dispatcher.metrics.nats_retransmit_events.get(), 0);
        assert_eq!(dispatcher.metrics.tcp_retransmits.get(), 1);
    }

    #[test]
    fn test_fd_limit_approaching_prunes() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::FdLimitApproaching {
            pid: 1,
            fd: 8000,
            current_fd_count: 8000,
            fd_soft_limit: 8192,
        });

        assert_eq!(*callbacks.pruned.lock().unwrap(), 1);
    }

    #[test]
    fn test_fd_hard_limit_approaching_activates_backpressure() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        // 95% of 8192 = ~7782
        dispatcher.dispatch(MonitorEvent::FdLimitApproaching {
            pid: 1,
            fd: 7800,
            current_fd_count: 7800,
            fd_soft_limit: 8192,
        });

        assert!(dispatcher.is_backpressure_active());
        assert_eq!(*callbacks.pruned.lock().unwrap(), 1);
    }

    #[test]
    fn test_memory_pressure_medium() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::MemPressure {
            pid: 1,
            free_pages: 50000,
            reclaim_pages: 1000,
            pressure_level: 1,
            anon_pages: 30000,
        });

        assert!(dispatcher.is_backpressure_active());
        assert_eq!(dispatcher.last_pressure_level(), 1);
        let pressure_events = callbacks.node_under_pressure.lock().unwrap();
        assert_eq!(pressure_events.len(), 1);
        assert_eq!(pressure_events[0], ("test-node".to_string(), 1));
    }

    #[test]
    fn test_memory_pressure_critical() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::MemPressure {
            pid: 1,
            free_pages: 10000,
            reclaim_pages: 5000,
            pressure_level: 2,
            anon_pages: 50000,
        });

        assert!(dispatcher.is_backpressure_active());
        assert!(dispatcher.is_degraded());
        assert_eq!(dispatcher.last_pressure_level(), 2);
        let pressure_events = callbacks.node_under_pressure.lock().unwrap();
        assert_eq!(pressure_events.len(), 1);
        assert_eq!(pressure_events[0], ("test-node".to_string(), 2));
    }

    #[test]
    fn test_memory_pressure_recovery() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        // First go to critical
        dispatcher.dispatch(MonitorEvent::MemPressure {
            pid: 1,
            free_pages: 10000,
            reclaim_pages: 5000,
            pressure_level: 2,
            anon_pages: 50000,
        });
        assert!(dispatcher.is_backpressure_active());

        // Then recover
        dispatcher.dispatch(MonitorEvent::MemPressure {
            pid: 1,
            free_pages: 100000,
            reclaim_pages: 0,
            pressure_level: 0,
            anon_pages: 10000,
        });
        assert!(!dispatcher.is_backpressure_active());

        let recovered = callbacks.node_pressure_recovered.lock().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0], "test-node");
    }

    #[test]
    fn test_disk_slow_io_enters_degraded_mode() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::DiskSlowIo {
            dev_major: 8,
            dev_minor: 0,
            latency_ns: 100_000_000, // 100ms
            io_type: 1,              // write
        });

        assert!(dispatcher.is_degraded());
        assert_eq!(
            dispatcher
                .metrics
                .disk_io_latency_seconds
                .get_sample_count(),
            1
        );
    }

    #[test]
    fn test_syscall_privilege_escalation() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::SyscallAnomaly {
            pid: 42,
            syscall_nr: 101, // SYS_PTRACE
            syscall_category: SyscallCategory::PrivilegeEscalation,
            count_in_window: 1,
        });

        assert_eq!(dispatcher.metrics.security_violations.get(), 1);
        let killed = callbacks.killed_instances.lock().unwrap();
        assert_eq!(killed.len(), 1);
        assert_eq!(killed[0].0, 42);

        let incidents = callbacks.security_incidents.lock().unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].1, 42);
        assert_eq!(incidents[0].2, 101);
        assert_eq!(incidents[0].3, "PrivilegeEscalation");
    }

    #[test]
    fn test_syscall_process_control() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::SyscallAnomaly {
            pid: 99,
            syscall_nr: 59, // SYS_EXECVE
            syscall_category: SyscallCategory::ProcessControl,
            count_in_window: 1,
        });

        assert_eq!(dispatcher.metrics.security_violations.get(), 1);
        let killed = callbacks.killed_instances.lock().unwrap();
        assert_eq!(killed.len(), 1);
        assert_eq!(killed[0].0, 99);
    }

    #[test]
    fn test_backpressure_deduplication() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        // Two OOM kills should only activate backpressure once
        dispatcher.dispatch(MonitorEvent::ProcessExit {
            pid: 100,
            ppid: 1,
            exit_code: 0,
            signal: 9,
            comm: [0; 16],
            cgroup_id: 0,
        });
        dispatcher.dispatch(MonitorEvent::ProcessExit {
            pid: 101,
            ppid: 1,
            exit_code: 0,
            signal: 9,
            comm: [0; 16],
            cgroup_id: 0,
        });

        // Backpressure should only be activated once (deduplication)
        assert_eq!(callbacks.backpressure_activations.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_exit_degraded_mode() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        // Enter degraded mode via disk slow I/O
        dispatcher.dispatch(MonitorEvent::DiskSlowIo {
            dev_major: 8,
            dev_minor: 0,
            latency_ns: 100_000_000,
            io_type: 1,
        });
        assert!(dispatcher.is_degraded());

        // Exit degraded mode
        dispatcher.exit_degraded_mode();
        assert!(!dispatcher.is_degraded());
        assert!(!dispatcher.is_backpressure_active());
    }

    #[test]
    fn test_tcp_connect_close_updates_connection_count() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::TcpConnect {
            pid: 1,
            src_port: 8080,
            dst_port: 443,
            old_state: 0,
            new_state: 1,
        });
        assert_eq!(dispatcher.metrics.tcp_connection_count.get(), 1);

        dispatcher.dispatch(MonitorEvent::TcpClose {
            pid: 1,
            src_port: 8080,
            dst_port: 443,
        });
        assert_eq!(dispatcher.metrics.tcp_connection_count.get(), 0);
    }

    #[test]
    fn test_events_processed_counter() {
        let callbacks = Arc::new(TestCallbacks::default());
        let dispatcher = make_dispatcher(callbacks.clone());

        dispatcher.dispatch(MonitorEvent::ProcessExec {
            pid: 1,
            ppid: 0,
            comm: [0; 16],
            cgroup_id: 0,
        });
        dispatcher.dispatch(MonitorEvent::TcpConnect {
            pid: 1,
            src_port: 80,
            dst_port: 443,
            old_state: 0,
            new_state: 1,
        });

        assert_eq!(dispatcher.metrics.events_processed.get(), 2);
    }

    #[test]
    fn test_monitor_event_type_mapping() {
        assert_eq!(
            MonitorEvent::ProcessExec {
                pid: 0,
                ppid: 0,
                comm: [0; 16],
                cgroup_id: 0
            }
            .event_type(),
            EventType::ProcessExec
        );
        assert_eq!(
            MonitorEvent::ProcessExit {
                pid: 0,
                ppid: 0,
                exit_code: 0,
                signal: 0,
                comm: [0; 16],
                cgroup_id: 0
            }
            .event_type(),
            EventType::ProcessExit
        );
        assert_eq!(
            MonitorEvent::TcpConnect {
                pid: 0,
                src_port: 0,
                dst_port: 0,
                old_state: 0,
                new_state: 0
            }
            .event_type(),
            EventType::TcpConnect
        );
        assert_eq!(
            MonitorEvent::TcpClose {
                pid: 0,
                src_port: 0,
                dst_port: 0
            }
            .event_type(),
            EventType::TcpClose
        );
        assert_eq!(
            MonitorEvent::TcpRetransmit {
                pid: 0,
                src_port: 0,
                dst_port: 0,
                retransmits: 0,
                rtt_us: 0
            }
            .event_type(),
            EventType::TcpRetransmit
        );
        assert_eq!(
            MonitorEvent::FdOpen {
                pid: 0,
                fd: 0,
                current_fd_count: 0,
                fd_soft_limit: 0
            }
            .event_type(),
            EventType::FdOpen
        );
        assert_eq!(
            MonitorEvent::FdLimitApproaching {
                pid: 0,
                fd: 0,
                current_fd_count: 0,
                fd_soft_limit: 0
            }
            .event_type(),
            EventType::FdLimitApproaching
        );
        assert_eq!(
            MonitorEvent::MemPressure {
                pid: 0,
                free_pages: 0,
                reclaim_pages: 0,
                pressure_level: 0,
                anon_pages: 0
            }
            .event_type(),
            EventType::MemPressure
        );
        assert_eq!(
            MonitorEvent::DiskSlowIo {
                dev_major: 0,
                dev_minor: 0,
                latency_ns: 0,
                io_type: 0
            }
            .event_type(),
            EventType::DiskSlowIo
        );
        assert_eq!(
            MonitorEvent::SyscallAnomaly {
                pid: 0,
                syscall_nr: 0,
                syscall_category: SyscallCategory::Normal,
                count_in_window: 0
            }
            .event_type(),
            EventType::SyscallAnomaly
        );
    }
}
