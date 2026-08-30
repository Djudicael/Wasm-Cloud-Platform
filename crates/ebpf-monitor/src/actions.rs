//! Recovery action executor for eBPF monitor events.
//!
//! `ActionDispatcher` receives parsed `MonitorEvent`s from the ring buffer
//! consumer or the userspace fallback and converts them into platform actions.
//! The event model and callback contract live in dedicated files so the
//! dispatch path stays readable.

#[path = "actions/callbacks.rs"]
mod callbacks;
#[path = "actions/model.rs"]
mod model;

use crate::metrics::EbpfMetrics;
use crate::namespace_map::NamespaceMap;
use crate::MonitorConfig;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

pub use callbacks::{EventCallbacks, NoopCallbacks};
pub use model::{MonitorEvent, NamespaceIncidentType, RecoveryAction};

#[derive(Default)]
struct SlowIoRecoveryState {
    active: bool,
    generation: u64,
    worker_running: bool,
}

/// Process monitor events and determine recovery actions.
///
/// The dispatcher updates metrics for every event, decides the recovery action,
/// and invokes the platform callbacks that execute it.
pub struct ActionDispatcher {
    /// Prometheus metrics, updated for every event regardless of callbacks.
    pub metrics: Arc<EbpfMetrics>,
    /// Platform callbacks for recovery actions.
    callbacks: Arc<dyn EventCallbacks>,
    /// Node ID for publishing cluster events.
    node_id: String,
    /// Whether backpressure is currently active.
    backpressure_active: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the node is in degraded mode.
    degraded_mode: Arc<std::sync::atomic::AtomicBool>,
    /// Whether critical memory pressure currently requires degraded mode.
    memory_pressure_degraded: Arc<std::sync::atomic::AtomicBool>,
    /// Slow-I/O cause and its single generation-fenced recovery worker.
    slow_io_recovery: Arc<std::sync::Mutex<SlowIoRecoveryState>>,
    /// Last memory pressure level, used to detect recovery.
    last_pressure_level: Arc<std::sync::atomic::AtomicU32>,
    /// Monotonic pressure-event generation used to fence recovery timers.
    pressure_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Current monitor configuration, updated through hot reload.
    config: std::sync::RwLock<MonitorConfig>,
    /// Namespace identity map for updating port-to-TID bindings from eBPF events.
    namespace_map: std::sync::RwLock<Option<Arc<NamespaceMap>>>,
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
            backpressure_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            degraded_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            memory_pressure_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            slow_io_recovery: Arc::new(std::sync::Mutex::new(SlowIoRecoveryState::default())),
            last_pressure_level: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            pressure_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config: std::sync::RwLock::new(MonitorConfig::default()),
            namespace_map: std::sync::RwLock::new(None),
        }
    }

    /// Set the namespace map for updating port-to-TID bindings.
    pub fn set_namespace_map(&self, ns_map: Arc<NamespaceMap>) {
        *self.namespace_map.write().unwrap() = Some(ns_map);
    }

    /// Create a dispatcher with no-op callbacks for tests or safe defaults.
    pub fn new_noop(metrics: Arc<EbpfMetrics>, node_id: String) -> Self {
        Self::new(metrics, Arc::new(NoopCallbacks), node_id)
    }

    /// Create a dispatcher with an explicit initial monitor configuration.
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
            backpressure_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            degraded_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            memory_pressure_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            slow_io_recovery: Arc::new(std::sync::Mutex::new(SlowIoRecoveryState::default())),
            last_pressure_level: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            pressure_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config: std::sync::RwLock::new(config),
            namespace_map: std::sync::RwLock::new(None),
        }
    }

    /// Update the monitor thresholds at runtime and return the previous config.
    pub fn update_thresholds(&self, new_config: MonitorConfig) -> MonitorConfig {
        let mut guard = self.config.write().unwrap_or_else(|e| e.into_inner());
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

    /// Read the current monitor configuration for introspection or admin APIs.
    pub fn current_config(&self) -> MonitorConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Dispatch a monitor event, update metrics, and trigger recovery actions.
    pub fn dispatch(&self, event: MonitorEvent) {
        self.metrics.events_processed.inc();
        self.metrics
            .events_by_type
            .with_label_values(&[event.event_type().as_str()])
            .inc();

        match event {
            MonitorEvent::ProcessExec {
                pid,
                tid,
                ppid,
                comm,
                cgroup_id,
            } => {
                let comm_str = String::from_utf8_lossy(
                    &comm[..comm.iter().position(|&b| b == 0).unwrap_or(comm.len())],
                );
                let (namespace, app_id) = self.identity_for_tid(tid);
                info!(
                    pid,
                    tid,
                    namespace = %namespace,
                    app_id = %app_id,
                    ppid,
                    comm = %comm_str,
                    cgroup_id,
                    "Process exec detected (child of wasm-node)"
                );
            }
            MonitorEvent::ProcessExit {
                pid,
                tid,
                ppid,
                exit_code,
                signal,
                comm,
                cgroup_id: _,
            } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                let comm_str = String::from_utf8_lossy(
                    &comm[..comm.iter().position(|&b| b == 0).unwrap_or(comm.len())],
                );

                if signal == 9 {
                    error!(
                        pid,
                        tid,
                        namespace = %namespace,
                        app_id = %app_id,
                        ppid,
                        comm = %comm_str,
                        "OOM kill detected for wasm-node child process"
                    );
                    self.metrics.oom_kills.inc();
                    self.callbacks.remove_from_upstream(pid);
                    self.activate_backpressure_if_needed("OOM kill detected");
                } else if signal != 0 {
                    warn!(
                        pid,
                        tid,
                        namespace = %namespace,
                        app_id = %app_id,
                        ppid,
                        signal,
                        exit_code,
                        comm = %comm_str,
                        "Wasm instance killed by signal"
                    );
                    self.metrics.signal_deaths.inc();
                    self.callbacks.remove_from_upstream(pid);
                } else {
                    info!(
                        pid,
                        tid,
                        namespace = %namespace,
                        app_id = %app_id,
                        ppid,
                        exit_code,
                        comm = %comm_str,
                        "Wasm instance exited normally"
                    );
                    self.callbacks.remove_from_upstream(pid);
                }
                self.metrics.process_exits.inc();
            }
            MonitorEvent::TcpConnect {
                pid,
                tid,
                src_port,
                dst_port,
                old_state: _,
                new_state: _,
            } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                debug!(pid, tid, namespace = %namespace, app_id = %app_id, src_port, dst_port, "TCP connection opened");
                self.metrics.tcp_connection_count.inc();
            }
            MonitorEvent::TcpClose {
                pid,
                tid,
                src_port,
                dst_port,
            } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                debug!(pid, tid, namespace = %namespace, app_id = %app_id, src_port, dst_port, "TCP connection closed");
                if self.metrics.tcp_connection_count.get() > 0 {
                    self.metrics.tcp_connection_count.dec();
                }
                self.callbacks
                    .tcp_connection_closed(tid, src_port, dst_port);
            }
            MonitorEvent::TcpRetransmit {
                pid,
                tid,
                src_port,
                dst_port,
                retransmits,
                rtt_us,
            } => {
                if dst_port == 4222 || src_port == 4222 {
                    warn!(
                        pid,
                        tid,
                        src_port,
                        dst_port,
                        retransmits,
                        rtt_us,
                        "NATS TCP retransmits detected - pre-emptive disconnect warning"
                    );
                    self.callbacks.mark_nats_disconnected();
                    self.metrics.nats_retransmit_events.inc();
                } else {
                    warn!(
                        pid,
                        tid, src_port, dst_port, retransmits, rtt_us, "TCP retransmits detected"
                    );
                }
                self.metrics.tcp_retransmits.inc();
            }
            MonitorEvent::TcpAccept { pid, tid, fd } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                debug!(pid, tid, namespace = %namespace, app_id = %app_id, fd, "TCP connection accepted");
            }
            MonitorEvent::TcpSend {
                pid,
                tid,
                fd,
                bytes,
            } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                debug!(pid, tid, namespace = %namespace, app_id = %app_id, fd, bytes, "TCP payload sent");
            }
            MonitorEvent::TcpReceive {
                pid,
                tid,
                fd,
                bytes,
            } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                debug!(pid, tid, namespace = %namespace, app_id = %app_id, fd, bytes, "TCP payload received");
            }
            MonitorEvent::FdOpen {
                pid,
                tid,
                fd,
                current_fd_count,
                fd_soft_limit,
            } => {
                self.metrics.set_fd_usage(current_fd_count, fd_soft_limit);
                let (namespace, app_id) = self.identity_for_tid(tid);
                debug!(pid, tid, namespace = %namespace, app_id = %app_id, fd, current_fd_count, fd_soft_limit, "FD opened");
            }
            MonitorEvent::FdLimitApproaching {
                pid,
                tid,
                fd,
                current_fd_count,
                fd_soft_limit,
            } => {
                let ratio = current_fd_count as f64 / fd_soft_limit as f64;
                if ratio > 0.95 {
                    error!(
                        pid,
                        tid,
                        fd,
                        current_fd_count,
                        fd_soft_limit,
                        "FD hard limit approaching - pruning idle instances immediately"
                    );
                    self.callbacks.prune_idle_instances();
                    self.activate_backpressure_if_needed("FD hard limit approaching");
                } else {
                    warn!(
                        pid,
                        tid,
                        fd,
                        current_fd_count,
                        fd_soft_limit,
                        "FD soft limit approaching - considering pruning idle instances"
                    );
                    self.callbacks.prune_idle_instances();
                }
                self.metrics.set_fd_usage(current_fd_count, fd_soft_limit);
            }
            MonitorEvent::FdClose {
                pid,
                tid,
                fd,
                current_fd_count,
            } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                self.metrics.fd_count.set(current_fd_count as i64);
                debug!(pid, tid, namespace = %namespace, app_id = %app_id, fd, current_fd_count, "FD closed");
            }
            MonitorEvent::MemPressure {
                pid,
                tid,
                free_pages,
                reclaim_pages,
                pressure_level,
                anon_pages,
            } => {
                let pressure_generation = self
                    .pressure_generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                let prev_level = self
                    .last_pressure_level
                    .swap(pressure_level, std::sync::atomic::Ordering::Relaxed);
                self.metrics.set_memory_pressure(pressure_level);

                match pressure_level {
                    0 => {
                        info!(
                            pid,
                            tid, free_pages, reclaim_pages, anon_pages, "Memory pressure: LOW"
                        );
                        if prev_level >= 1 {
                            self.deactivate_backpressure_if_needed();
                            self.callbacks
                                .publish_node_pressure_recovered(&self.node_id);
                        }
                        self.memory_pressure_degraded
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        self.refresh_degraded_mode("Memory pressure recovered");
                    }
                    1 => {
                        warn!(
                            pid,
                            tid,
                            free_pages,
                            reclaim_pages,
                            anon_pages,
                            "Memory pressure: MEDIUM - pruning idle instances"
                        );
                        self.callbacks.prune_idle_instances();
                        self.activate_backpressure_if_needed("Memory pressure: MEDIUM");
                        self.callbacks.publish_node_under_pressure(&self.node_id, 1);
                        self.schedule_memory_pressure_recovery(pressure_generation);
                    }
                    2 => {
                        error!(
                            pid,
                            tid,
                            free_pages,
                            reclaim_pages,
                            anon_pages,
                            "Memory pressure: CRITICAL - killing largest instance"
                        );
                        self.callbacks.prune_idle_instances();
                        self.activate_backpressure_if_needed("Memory pressure: CRITICAL");
                        self.callbacks.publish_node_under_pressure(&self.node_id, 2);
                        self.memory_pressure_degraded
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        self.refresh_degraded_mode("Memory pressure: CRITICAL");
                    }
                    _ => {
                        warn!(pressure_level, free_pages, "Unknown memory pressure level");
                    }
                }
            }
            MonitorEvent::DiskSlowIo {
                pid,
                tid,
                dev_major,
                dev_minor,
                sector,
                nr_sector,
                bytes,
                latency_ns,
                cgroup_id,
                io_type,
            } => {
                let latency_ms = latency_ns as f64 / 1_000_000.0;
                let io_type_str = match io_type {
                    0 => "read",
                    1 => "write",
                    2 => "sync",
                    _ => "unknown",
                };
                let (namespace, app_id) = self.identity_for_tid(tid);
                let monitored_workload = app_id != "<unregistered>";
                warn!(
                    pid,
                    tid,
                    cgroup_id,
                    monitored_workload,
                    namespace = %namespace,
                    app_id = %app_id,
                    dev = format!("{}:{}", dev_major, dev_minor),
                    sector,
                    nr_sector,
                    bytes,
                    latency_ms,
                    io_type = io_type_str,
                    "Slow disk I/O detected"
                );
                self.metrics.observe_disk_latency_ns(latency_ns);
                self.metrics.add_disk_io_bytes(io_type_str, bytes);
                self.mark_slow_io_degraded(&format!(
                    "Slow disk I/O on {}:{} ({:.1}ms)",
                    dev_major, dev_minor, latency_ms
                ));
            }
            MonitorEvent::SyscallAnomaly {
                pid,
                tid,
                syscall_nr,
                syscall_category,
                count_in_window,
            } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                match syscall_category {
                    crate::common::SyscallCategory::PrivilegeEscalation => {
                        error!(
                            pid,
                            tid,
                            namespace = %namespace,
                            app_id = %app_id,
                            syscall_nr,
                            count_in_window,
                            "SECURITY: Privilege escalation syscall from Wasm instance!"
                        );
                        self.metrics.security_violations.inc();
                        self.callbacks
                            .kill_instance_by_tid(tid, "Privilege escalation syscall detected");
                        self.callbacks.publish_security_incident(
                            &self.node_id,
                            pid,
                            syscall_nr,
                            "PrivilegeEscalation",
                        );
                    }
                    crate::common::SyscallCategory::ProcessControl => {
                        error!(
                            pid,
                            tid,
                            namespace = %namespace,
                            app_id = %app_id,
                            syscall_nr,
                            count_in_window,
                            "SECURITY: Process control syscall from Wasm instance!"
                        );
                        self.metrics.security_violations.inc();
                        self.callbacks
                            .kill_instance_by_tid(tid, "Process control syscall detected");
                        self.callbacks.publish_security_incident(
                            &self.node_id,
                            pid,
                            syscall_nr,
                            "ProcessControl",
                        );
                    }
                    crate::common::SyscallCategory::NetworkControl => {
                        warn!(
                            pid,
                            tid,
                            namespace = %namespace,
                            app_id = %app_id,
                            syscall_nr,
                            count_in_window,
                            "Unexpected network control syscall from Wasm instance"
                        );
                        self.metrics.security_violations.inc();
                    }
                    crate::common::SyscallCategory::Normal => {
                        warn!(
                            pid,
                            tid,
                            namespace = %namespace,
                            app_id = %app_id,
                            syscall_nr,
                            count_in_window,
                            "High syscall rate from Wasm instance"
                        );
                    }
                }
            }
            MonitorEvent::SyscallActivity {
                pid,
                tid,
                syscall_nr,
                count_in_window,
            } => {
                let (namespace, app_id) = self.identity_for_tid(tid);
                tracing::info!(
                    pid,
                    tid,
                    namespace = %namespace,
                    app_id = %app_id,
                    syscall_nr,
                    count_in_window,
                    "Known syscall activity from monitored workload"
                );
            }
            MonitorEvent::TidConnection {
                tid,
                namespace: _,
                app_id: _,
                source_port,
            } => {
                tracing::debug!(tid, source_port, "TID connected to gateway");
                if let Some(ref ns_map) = *self.namespace_map.read().unwrap() {
                    ns_map.bind_port(source_port, tid);
                }
            }
            MonitorEvent::TidDisconnection {
                tid: _,
                source_port,
            } => {
                tracing::debug!(source_port, "TID disconnected from gateway");
                if let Some(ref ns_map) = *self.namespace_map.read().unwrap() {
                    ns_map.unbind_port(source_port);
                }
            }
            MonitorEvent::NamespaceAudit {
                tid,
                namespace,
                app_id,
            } => {
                tracing::info!(
                    tid,
                    namespace = %namespace,
                    app_id = %app_id,
                    "Namespace audit event"
                );
            }
            MonitorEvent::NamespaceForgedHeader {
                tid,
                namespace,
                app_id,
            } => {
                error!(
                    tid,
                    namespace = %namespace,
                    app_id = %app_id,
                    "SECURITY: Forged namespace header detected"
                );
                self.metrics.security_violations.inc();
                self.callbacks
                    .kill_instance_by_tid(tid, "Forged namespace header detected by eBPF audit");
            }
            MonitorEvent::UnregisteredTidConnection { tid } => {
                warn!(
                    tid,
                    "Unregistered TID connected to gateway - possible bypass attempt"
                );
                self.metrics.security_violations.inc();
            }
        }
    }

    fn identity_for_tid(&self, tid: u32) -> (String, String) {
        let map = self.namespace_map.read().unwrap();
        map.as_ref()
            .and_then(|map| map.lookup_event_identity(tid))
            .map(|identity| {
                (
                    identity.namespace_str().to_string(),
                    identity.app_id_str().to_string(),
                )
            })
            .unwrap_or_else(|| ("<unregistered>".to_string(), "<unregistered>".to_string()))
    }

    fn activate_backpressure_if_needed(&self, reason: &str) {
        if !self
            .backpressure_active
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.callbacks.activate_backpressure(reason);
        }
    }

    fn deactivate_backpressure_if_needed(&self) {
        if self
            .backpressure_active
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.callbacks.deactivate_backpressure();
        }
    }

    fn schedule_memory_pressure_recovery(&self, generation: u64) {
        let pressure_generation = Arc::clone(&self.pressure_generation);
        let last_pressure_level = Arc::clone(&self.last_pressure_level);
        let backpressure_active = Arc::clone(&self.backpressure_active);
        let metrics = Arc::clone(&self.metrics);
        let callbacks = Arc::clone(&self.callbacks);
        let node_id = self.node_id.clone();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            if pressure_generation.load(std::sync::atomic::Ordering::Relaxed) != generation
                || last_pressure_level.load(std::sync::atomic::Ordering::Relaxed) != 1
            {
                return;
            }

            last_pressure_level.store(0, std::sync::atomic::Ordering::Relaxed);
            metrics.set_memory_pressure(0);
            if backpressure_active.swap(false, std::sync::atomic::Ordering::Relaxed) {
                callbacks.deactivate_backpressure();
            }
            callbacks.publish_node_pressure_recovered(&node_id);
            info!("Memory pressure cooldown elapsed - accepting traffic again");
        });
    }

    fn mark_slow_io_degraded(&self, reason: &str) {
        self.mark_slow_io_degraded_for(reason, Duration::from_secs(30));
    }

    fn mark_slow_io_degraded_for(&self, reason: &str, cooldown: Duration) {
        let should_start_worker = {
            let mut recovery = self
                .slow_io_recovery
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            recovery.active = true;
            recovery.generation = recovery.generation.wrapping_add(1);
            if recovery.worker_running {
                false
            } else {
                recovery.worker_running = true;
                true
            }
        };
        self.refresh_degraded_mode(reason);

        if !should_start_worker {
            return;
        }

        let slow_io_recovery = Arc::clone(&self.slow_io_recovery);
        let memory_pressure_degraded = Arc::clone(&self.memory_pressure_degraded);
        let degraded_mode = Arc::clone(&self.degraded_mode);
        std::thread::spawn(move || loop {
            let observed_generation = {
                let recovery = slow_io_recovery
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                recovery.generation
            };
            std::thread::sleep(cooldown);

            let mut recovery = slow_io_recovery
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !recovery.active {
                recovery.worker_running = false;
                return;
            }
            if recovery.generation != observed_generation {
                continue;
            }

            recovery.active = false;
            recovery.worker_running = false;
            if !memory_pressure_degraded.load(std::sync::atomic::Ordering::Relaxed)
                && degraded_mode.swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                info!("Slow-I/O cooldown elapsed - exiting degraded mode");
            }
            return;
        });
    }

    fn refresh_degraded_mode(&self, reason: &str) {
        let should_be_degraded = self
            .memory_pressure_degraded
            .load(std::sync::atomic::Ordering::Relaxed)
            || self
                .slow_io_recovery
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .active;
        let was_degraded = self
            .degraded_mode
            .swap(should_be_degraded, std::sync::atomic::Ordering::Relaxed);
        if should_be_degraded && !was_degraded {
            warn!(reason, "Entering degraded mode");
        } else if !should_be_degraded && was_degraded {
            info!(reason, "Exiting degraded mode - recovered");
        }
    }

    /// Exit degraded mode if currently active.
    pub fn exit_degraded_mode(&self) {
        self.memory_pressure_degraded
            .store(false, std::sync::atomic::Ordering::Relaxed);
        {
            let mut recovery = self
                .slow_io_recovery
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            recovery.active = false;
            recovery.generation = recovery.generation.wrapping_add(1);
        }
        if self
            .degraded_mode
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            info!("Exiting degraded mode - recovered");
            self.deactivate_backpressure_if_needed();
        }
    }

    pub fn is_backpressure_active(&self) -> bool {
        self.backpressure_active
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded_mode
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn last_pressure_level(&self) -> u32 {
        self.last_pressure_level
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
#[path = "actions/tests.rs"]
mod tests;
