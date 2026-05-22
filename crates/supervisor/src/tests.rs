use crate::instance::wait_for_ready;
use crate::instance::{BillingInfo, ManagedInstance, PolicyCounterSnapshot};
use crate::is_instance_bind_allowed;
use crate::network::LocalServiceRegistry;
use crate::port_alloc::PortAllocator;
use common::types::AppId;
use runtime::policy_tracker::PolicyCounters;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tokio::net::TcpListener;

use crate::deployment::RollbackPolicy;
use crate::Supervisor;

#[tokio::test]
async fn test_deployment_hot_swap_basics() {
    // This is a placeholder test for hot_swap logic
    // The actual deployment logic is verified via integration tests
    let policy = RollbackPolicy::default();
    assert_eq!(policy.health_failure_threshold, 3);
    assert_eq!(policy.observation_window, Duration::from_secs(30));
}

#[test]
fn test_port_allocator_basic() {
    let addr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let allocator = PortAllocator::new(addr, 10000, 10002); // 3 ports

    // allocate() returns a different port on each call
    let p1 = allocator.allocate().unwrap();
    let p2 = allocator.allocate().unwrap();
    let p3 = allocator.allocate().unwrap();

    assert_ne!(p1, p2);
    assert_ne!(p2, p3);
    assert_ne!(p1, p3);

    // allocate() returns Err when the pool is exhausted (not a panic)
    let err = allocator.allocate();
    assert!(err.is_err());

    // release(port) returns the port to the pool so it can be re-allocated
    allocator.release(p2);

    // A port released by one instance can be re-allocated to a new instance
    let p4 = allocator.allocate().unwrap();
    assert_eq!(p2, p4);
}

#[test]
fn test_port_allocator_threading() {
    let addr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let allocator = Arc::new(PortAllocator::new(addr, 20000, 20050)); // 51 ports

    // allocate() and release() are safe to call from multiple threads simultaneously
    let mut handles = vec![];
    for _ in 0..20 {
        let alloc_clone = allocator.clone();
        handles.push(thread::spawn(move || {
            let p = alloc_clone.allocate().unwrap();
            // simulate some work
            thread::sleep(Duration::from_millis(2));
            alloc_clone.release(p);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Ensure we can still allocate all 51 ports after the concurrent thrashing
    for _ in 0..51 {
        assert!(allocator.allocate().is_ok());
    }
    assert!(allocator.allocate().is_err());
}

#[tokio::test]
async fn test_local_service_registry() {
    let registry = LocalServiceRegistry::default();
    let app_id = AppId::new("test-app", "v1");
    let addr = "127.0.0.1:8080".parse().unwrap();

    // Resolving an unknown app returns None (not an error)
    assert!(registry.resolve("default", "test-app").await.is_none());

    // LocalServiceRegistry::register() stores an address for an app
    registry.register(&app_id, addr).await;

    // LocalServiceRegistry::resolve() returns the stored address
    assert_eq!(registry.resolve("default", "test-app").await, Some(addr));

    // LocalServiceRegistry::deregister() removes the address
    registry.deregister(&app_id, &addr).await;
    assert!(registry.resolve("default", "test-app").await.is_none());
}

#[tokio::test]
async fn test_wait_for_ready_success() {
    // Bind a listener so the port is open
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // wait_for_ready() returns Ok within 500ms when the port opens
    let result = wait_for_ready(addr, Duration::from_millis(500)).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_wait_for_ready_timeout() {
    // Bind and immediately drop to guarantee we have an unused port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // wait_for_ready() returns Err after the timeout when the port never opens
    let result = wait_for_ready(addr, Duration::from_millis(50)).await;
    assert!(result.is_err());

    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("did not become ready"));
}

#[test]
fn test_instance_bind_policy_allows_expected_loopback_port() {
    let mut allowed_ports = std::collections::HashSet::new();
    allowed_ports.insert(18080);

    assert!(is_instance_bind_allowed(
        "127.0.0.1:18080".parse().unwrap(),
        &allowed_ports,
        "127.0.0.1".parse().unwrap(),
    ));
}

#[test]
fn test_instance_bind_policy_rejects_wildcard_bind_even_on_allowed_port() {
    let mut allowed_ports = std::collections::HashSet::new();
    allowed_ports.insert(18080);

    assert!(!is_instance_bind_allowed(
        "0.0.0.0:18080".parse().unwrap(),
        &allowed_ports,
        "127.0.0.1".parse().unwrap(),
    ));
}

#[test]
fn test_instance_bind_policy_rejects_disallowed_port() {
    let mut allowed_ports = std::collections::HashSet::new();
    allowed_ports.insert(18080);

    assert!(!is_instance_bind_allowed(
        "127.0.0.1:18081".parse().unwrap(),
        &allowed_ports,
        "127.0.0.1".parse().unwrap(),
    ));
}

#[test]
fn test_instance_bind_policy_supports_ipv6_loopback() {
    let mut allowed_ports = std::collections::HashSet::new();
    allowed_ports.insert(18080);

    assert!(is_instance_bind_allowed(
        "[::1]:18080".parse().unwrap(),
        &allowed_ports,
        "::1".parse().unwrap(),
    ));
}

// ── SupervisorCommand Channel Tests ──────────────────────────────────────────

use crate::SupervisorCommand;

#[tokio::test]
async fn test_supervisor_command_channel_send_recv() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SupervisorCommand>(256);

    // Send a KillLargestInstance command
    tx.try_send(SupervisorCommand::KillLargestInstance {
        reason: "test OOM".to_string(),
    })
    .unwrap();

    // Receive and verify
    let cmd = rx.recv().await.unwrap();
    match cmd {
        SupervisorCommand::KillLargestInstance { reason } => {
            assert_eq!(reason, "test OOM");
        }
        _ => panic!("expected KillLargestInstance, got {:?}", cmd),
    }
}

#[tokio::test]
async fn test_supervisor_command_channel_prune_idle() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SupervisorCommand>(256);

    tx.try_send(SupervisorCommand::PruneIdleInstances {
        idle_threshold_secs: 120,
    })
    .unwrap();

    let cmd = rx.recv().await.unwrap();
    match cmd {
        SupervisorCommand::PruneIdleInstances {
            idle_threshold_secs,
        } => {
            assert_eq!(idle_threshold_secs, 120);
        }
        _ => panic!("expected PruneIdleInstances, got {:?}", cmd),
    }
}

#[tokio::test]
async fn test_supervisor_command_channel_remove_from_upstream() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SupervisorCommand>(256);

    let app_id = AppId("test-app".to_string());
    tx.try_send(SupervisorCommand::RemoveAppFromUpstream {
        app_id: app_id.clone(),
    })
    .unwrap();

    let cmd = rx.recv().await.unwrap();
    match cmd {
        SupervisorCommand::RemoveAppFromUpstream { app_id } => {
            assert_eq!(app_id.0, "test-app");
        }
        _ => panic!("expected RemoveAppFromUpstream, got {:?}", cmd),
    }
}

#[tokio::test]
async fn test_supervisor_command_channel_kill_instance() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SupervisorCommand>(256);

    let app_id = AppId("my-app".to_string());
    let instance_id = common::types::InstanceId::new();
    let instance_id_str = instance_id.0.to_string();
    tx.try_send(SupervisorCommand::KillInstance {
        app_id: app_id.clone(),
        instance_id: instance_id.clone(),
        reason: "security violation".to_string(),
    })
    .unwrap();

    let cmd = rx.recv().await.unwrap();
    match cmd {
        SupervisorCommand::KillInstance {
            app_id,
            instance_id,
            reason,
        } => {
            assert_eq!(app_id.0, "my-app");
            assert_eq!(instance_id.0.to_string(), instance_id_str);
            assert_eq!(reason, "security violation");
        }
        _ => panic!("expected KillInstance, got {:?}", cmd),
    }
}

#[tokio::test]
async fn test_supervisor_command_channel_multiple_commands() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SupervisorCommand>(256);

    // Send multiple commands in sequence
    tx.try_send(SupervisorCommand::KillLargestInstance {
        reason: "OOM".to_string(),
    })
    .unwrap();
    tx.try_send(SupervisorCommand::PruneIdleInstances {
        idle_threshold_secs: 30,
    })
    .unwrap();
    tx.try_send(SupervisorCommand::KillLargestInstance {
        reason: "FD exhaustion".to_string(),
    })
    .unwrap();

    // Verify all three arrive in order
    let cmd1 = rx.recv().await.unwrap();
    let cmd2 = rx.recv().await.unwrap();
    let cmd3 = rx.recv().await.unwrap();

    match cmd1 {
        SupervisorCommand::KillLargestInstance { reason } => {
            assert_eq!(reason, "OOM");
        }
        _ => panic!("expected KillLargestInstance as first command"),
    }
    match cmd2 {
        SupervisorCommand::PruneIdleInstances {
            idle_threshold_secs,
        } => {
            assert_eq!(idle_threshold_secs, 30);
        }
        _ => panic!("expected PruneIdleInstances as second command"),
    }
    match cmd3 {
        SupervisorCommand::KillLargestInstance { reason } => {
            assert_eq!(reason, "FD exhaustion");
        }
        _ => panic!("expected KillLargestInstance as third command"),
    }
}

#[tokio::test]
async fn test_supervisor_command_channel_backpressure() {
    // Channel with capacity 2 — third send should fail
    let (tx, _rx) = tokio::sync::mpsc::channel::<SupervisorCommand>(2);

    tx.try_send(SupervisorCommand::KillLargestInstance {
        reason: "1".to_string(),
    })
    .unwrap();
    tx.try_send(SupervisorCommand::KillLargestInstance {
        reason: "2".to_string(),
    })
    .unwrap();

    // Third send should fail (channel full)
    let result = tx.try_send(SupervisorCommand::KillLargestInstance {
        reason: "3".to_string(),
    });
    assert!(result.is_err());
}

fn dummy_managed_instance_with_counters(counters: Arc<PolicyCounters>) -> ManagedInstance {
    ManagedInstance {
        id: common::types::InstanceId::new(),
        app_id: AppId("test-app:v1".to_string()),
        addr: "127.0.0.1:1".parse().unwrap(),
        state: common::types::InstanceState::Ready {
            addr: "127.0.0.1:1".parse().unwrap(),
        },
        spawned_at: Instant::now(),
        last_request_at: Instant::now(),
        request_count: 0,
        task: None,
        shutdown_tx: None,
        billing_info: BillingInfo {
            tenant_id: "tenant-a".to_string(),
            fuel_quota: 100,
            ram_bytes: 2048,
        },
        tid: None,
        policy_counters: Some(counters),
        last_policy_export: PolicyCounterSnapshot::default(),
    }
}

#[test]
fn test_policy_metrics_export_flushes_deltas_once() {
    let policy_metrics = metrics::exporter::Metrics::new().policy;
    let counters = Arc::new(PolicyCounters::new());
    counters.connection_denied_total.store(2, Ordering::Relaxed);
    counters.egress_denied_total.store(3, Ordering::Relaxed);
    counters.fd_denied_total.store(5, Ordering::Relaxed);
    counters.fs_write_denied_total.store(7, Ordering::Relaxed);
    counters.bind_denied_total.store(11, Ordering::Relaxed);
    counters.dns_denied_total.store(13, Ordering::Relaxed);
    counters
        .outbound_connections_active
        .store(17, Ordering::Relaxed);
    counters.open_fds.store(19, Ordering::Relaxed);
    counters.current_memory_bytes.store(23, Ordering::Relaxed);
    counters.current_table_elements.store(29, Ordering::Relaxed);
    counters
        .memory_growth_denied_total
        .store(31, Ordering::Relaxed);
    counters
        .table_growth_denied_total
        .store(37, Ordering::Relaxed);

    let mut instances = vec![dummy_managed_instance_with_counters(counters.clone())];
    Supervisor::export_policy_metrics(&policy_metrics, &mut instances);
    Supervisor::export_policy_metrics(&policy_metrics, &mut instances);

    assert_eq!(policy_metrics.connection_denied_total.get(), 2);
    assert_eq!(policy_metrics.egress_denied_total.get(), 3);
    assert_eq!(policy_metrics.fd_denied_total.get(), 5);
    assert_eq!(policy_metrics.fs_write_denied_total.get(), 7);
    assert_eq!(policy_metrics.bind_denied_total.get(), 11);
    assert_eq!(policy_metrics.dns_denied_total.get(), 13);
    assert_eq!(policy_metrics.memory_growth_denied_total.get(), 31);
    assert_eq!(policy_metrics.table_growth_denied_total.get(), 37);
    assert_eq!(policy_metrics.active_outbound_connections.get(), 17);
    assert_eq!(policy_metrics.open_fds.get(), 19);
    assert_eq!(policy_metrics.current_memory_bytes.get(), 23);
    assert_eq!(policy_metrics.current_table_elements.get(), 29);
}

#[test]
fn test_policy_metrics_export_aggregates_live_gauges_across_instances() {
    let policy_metrics = metrics::exporter::Metrics::new().policy;
    let counters_a = Arc::new(PolicyCounters::new());
    counters_a
        .outbound_connections_active
        .store(2, Ordering::Relaxed);
    counters_a.open_fds.store(3, Ordering::Relaxed);
    counters_a.current_memory_bytes.store(5, Ordering::Relaxed);
    counters_a
        .current_table_elements
        .store(7, Ordering::Relaxed);

    let counters_b = Arc::new(PolicyCounters::new());
    counters_b
        .outbound_connections_active
        .store(5, Ordering::Relaxed);
    counters_b.open_fds.store(7, Ordering::Relaxed);
    counters_b.current_memory_bytes.store(11, Ordering::Relaxed);
    counters_b
        .current_table_elements
        .store(13, Ordering::Relaxed);

    let mut instances = vec![
        dummy_managed_instance_with_counters(counters_a),
        dummy_managed_instance_with_counters(counters_b),
    ];
    Supervisor::export_policy_metrics(&policy_metrics, &mut instances);

    assert_eq!(policy_metrics.active_outbound_connections.get(), 7);
    assert_eq!(policy_metrics.open_fds.get(), 10);
    assert_eq!(policy_metrics.current_memory_bytes.get(), 16);
    assert_eq!(policy_metrics.current_table_elements.get(), 20);
}

#[tokio::test]
async fn test_supervisor_command_channel_closed() {
    let (tx, rx) = tokio::sync::mpsc::channel::<SupervisorCommand>(256);
    drop(rx);

    // Sending to a closed channel should fail
    let result = tx.try_send(SupervisorCommand::KillLargestInstance {
        reason: "test".to_string(),
    });
    assert!(result.is_err());
}

#[test]
fn test_supervisor_command_debug_format() {
    let cmd = SupervisorCommand::KillLargestInstance {
        reason: "OOM".to_string(),
    };
    let debug_str = format!("{:?}", cmd);
    assert!(debug_str.contains("KillLargestInstance"));
    assert!(debug_str.contains("OOM"));

    let cmd = SupervisorCommand::PruneIdleInstances {
        idle_threshold_secs: 60,
    };
    let debug_str = format!("{:?}", cmd);
    assert!(debug_str.contains("PruneIdleInstances"));
    assert!(debug_str.contains("60"));
}
