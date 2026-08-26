use super::*;
use crate::common::{EventType, SyscallCategory};
use prometheus::Registry;
use std::sync::{Arc, Mutex};

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
    fn publish_security_incident(&self, node_id: &str, pid: u32, syscall_nr: u64, category: &str) {
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
    fn kill_instance_by_tid(&self, tid: u32, reason: &str) {
        self.killed_instances
            .lock()
            .unwrap()
            .push((tid, reason.to_string()));
    }
}

fn make_dispatcher(callbacks: Arc<TestCallbacks>) -> ActionDispatcher {
    let registry = Registry::new();
    let metrics = Arc::new(EbpfMetrics::new(&registry));
    ActionDispatcher::new(metrics, callbacks, "test-node".to_string())
}

#[test]
fn test_dispatcher_resolves_registered_application_identity_by_tid() {
    let callbacks = Arc::new(TestCallbacks::default());
    let dispatcher = make_dispatcher(callbacks);
    let namespace_map = Arc::new(NamespaceMap::new_fallback());
    namespace_map
        .register_tid(
            420,
            crate::common::TidIdentity::new("oidc", "oidc-backend:v1"),
        )
        .unwrap();
    dispatcher.set_namespace_map(namespace_map);

    assert_eq!(
        dispatcher.identity_for_tid(420),
        ("oidc".to_string(), "oidc-backend:v1".to_string())
    );
}

#[test]
fn test_oom_kill_triggers_backpressure_and_removal() {
    let callbacks = Arc::new(TestCallbacks::default());
    let dispatcher = make_dispatcher(callbacks.clone());

    dispatcher.dispatch(MonitorEvent::ProcessExit {
        pid: 1234,
        tid: 1234,
        ppid: 1,
        exit_code: 0,
        signal: 9,
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
        tid: 5678,
        ppid: 1,
        exit_code: 1,
        signal: 6,
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
        tid: 9999,
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
        tid: 1,
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
        tid: 1,
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
        tid: 1,
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

    dispatcher.dispatch(MonitorEvent::FdLimitApproaching {
        pid: 1,
        tid: 1,
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
        tid: 1,
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
        tid: 1,
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

    dispatcher.dispatch(MonitorEvent::MemPressure {
        pid: 1,
        tid: 1,
        free_pages: 10000,
        reclaim_pages: 5000,
        pressure_level: 2,
        anon_pages: 50000,
    });
    assert!(dispatcher.is_backpressure_active());

    dispatcher.dispatch(MonitorEvent::MemPressure {
        pid: 1,
        tid: 1,
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
        pid: 0,
        tid: 0,
        dev_major: 8,
        dev_minor: 0,
        sector: 0,
        nr_sector: 0,
        bytes: 4096,
        latency_ns: 100_000_000,
        cgroup_id: 42,
        io_type: 1,
    });

    assert!(dispatcher.is_degraded());
    assert_eq!(
        dispatcher
            .metrics
            .disk_io_latency_seconds
            .get_sample_count(),
        1
    );
    assert_eq!(
        dispatcher
            .metrics
            .disk_io_bytes
            .with_label_values(&["write"])
            .get(),
        4096
    );
}

#[test]
fn test_syscall_privilege_escalation() {
    let callbacks = Arc::new(TestCallbacks::default());
    let dispatcher = make_dispatcher(callbacks.clone());

    dispatcher.dispatch(MonitorEvent::SyscallAnomaly {
        pid: 42,
        tid: 420,
        syscall_nr: 101,
        syscall_category: SyscallCategory::PrivilegeEscalation,
        count_in_window: 1,
    });

    assert_eq!(dispatcher.metrics.security_violations.get(), 1);
    let killed = callbacks.killed_instances.lock().unwrap();
    assert_eq!(killed.len(), 1);
    assert_eq!(killed[0].0, 420);

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
        tid: 990,
        syscall_nr: 59,
        syscall_category: SyscallCategory::ProcessControl,
        count_in_window: 1,
    });

    assert_eq!(dispatcher.metrics.security_violations.get(), 1);
    let killed = callbacks.killed_instances.lock().unwrap();
    assert_eq!(killed.len(), 1);
    assert_eq!(killed[0].0, 990);
}

#[test]
fn test_backpressure_deduplication() {
    let callbacks = Arc::new(TestCallbacks::default());
    let dispatcher = make_dispatcher(callbacks.clone());

    dispatcher.dispatch(MonitorEvent::ProcessExit {
        pid: 100,
        tid: 100,
        ppid: 1,
        exit_code: 0,
        signal: 9,
        comm: [0; 16],
        cgroup_id: 0,
    });
    dispatcher.dispatch(MonitorEvent::ProcessExit {
        pid: 101,
        tid: 101,
        ppid: 1,
        exit_code: 0,
        signal: 9,
        comm: [0; 16],
        cgroup_id: 0,
    });

    assert_eq!(callbacks.backpressure_activations.lock().unwrap().len(), 1);
}

#[test]
fn test_exit_degraded_mode() {
    let callbacks = Arc::new(TestCallbacks::default());
    let dispatcher = make_dispatcher(callbacks.clone());

    dispatcher.dispatch(MonitorEvent::DiskSlowIo {
        pid: 0,
        tid: 0,
        dev_major: 8,
        dev_minor: 0,
        sector: 0,
        nr_sector: 0,
        bytes: 4096,
        latency_ns: 100_000_000,
        cgroup_id: 42,
        io_type: 1,
    });
    assert!(dispatcher.is_degraded());

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
        tid: 1,
        src_port: 8080,
        dst_port: 443,
        old_state: 0,
        new_state: 1,
    });
    assert_eq!(dispatcher.metrics.tcp_connection_count.get(), 1);

    dispatcher.dispatch(MonitorEvent::TcpClose {
        pid: 1,
        tid: 1,
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
        tid: 1,
        ppid: 0,
        comm: [0; 16],
        cgroup_id: 0,
    });
    dispatcher.dispatch(MonitorEvent::TcpConnect {
        pid: 1,
        tid: 1,
        src_port: 80,
        dst_port: 443,
        old_state: 0,
        new_state: 1,
    });

    assert_eq!(dispatcher.metrics.events_processed.get(), 2);
    assert_eq!(
        dispatcher
            .metrics
            .events_by_type
            .with_label_values(&["process_start"])
            .get(),
        1
    );
    assert_eq!(
        dispatcher
            .metrics
            .events_by_type
            .with_label_values(&["tcp_connect"])
            .get(),
        1
    );
}

#[test]
fn test_monitor_event_type_mapping() {
    assert_eq!(
        MonitorEvent::ProcessExec {
            pid: 0,
            tid: 0,
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
            tid: 0,
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
            tid: 0,
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
            tid: 0,
            src_port: 0,
            dst_port: 0
        }
        .event_type(),
        EventType::TcpClose
    );
    assert_eq!(
        MonitorEvent::TcpRetransmit {
            pid: 0,
            tid: 0,
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
            tid: 0,
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
            tid: 0,
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
            tid: 0,
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
            pid: 0,
            tid: 0,
            dev_major: 0,
            dev_minor: 0,
            sector: 0,
            nr_sector: 0,
            bytes: 0,
            latency_ns: 0,
            cgroup_id: 0,
            io_type: 0
        }
        .event_type(),
        EventType::DiskSlowIo
    );
    assert_eq!(
        MonitorEvent::SyscallAnomaly {
            pid: 0,
            tid: 0,
            syscall_nr: 0,
            syscall_category: SyscallCategory::Normal,
            count_in_window: 0
        }
        .event_type(),
        EventType::SyscallAnomaly
    );
}
