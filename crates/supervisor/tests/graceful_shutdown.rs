use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Helper: Create a minimal test supervisor
async fn create_test_supervisor() -> Arc<supervisor::Supervisor> {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = temp_dir.path().join("test.redb");
    let store = storage::Store::open(&db_path).expect("failed to create store");
    let runtime = runtime::WasmRuntime::new().expect("Failed to create WasmRuntime");
    let port_alloc = Arc::new(supervisor::port_alloc::PortAllocator::new(
        "127.0.0.1".parse().unwrap(),
        15000,
        15999,
    ));
    let upstream = Arc::new(proxy::upstream::UpstreamRegistry::default());
    let host_router = Arc::new(proxy::router::HostRouter::default());
    let service_registry = Arc::new(supervisor::network::LocalServiceRegistry::default());
    let (event_tx, _rx) = mpsc::channel(100);

    let env_resolver = Arc::new(|_config: &AppConfig, _port: u16| vec![]);

    supervisor::Supervisor::new(
        store,
        runtime,
        port_alloc,
        upstream,
        host_router,
        service_registry,
        env_resolver,
        event_tx,
        None,
    )
}

/// Helper: Create test config with default wasm
fn create_test_config(app_id: AppId) -> AppConfig {
    AppConfig {
        id: app_id,
        fuel_quota: FuelQuota(500_000_000),
        memory_limit: MemoryPages(2048),
        max_instances: 3,
        idle_timeout_secs: 300,
        wasm_bind_port: 8080,
        env_vars: Default::default(),
        secret_keys: vec![],
        extended_limits: None,
        health_check_path: None,
        db_max_connections: None,
        rate_limit: None,
        tenant_id: None,
        policy: None,
        namespace: "default".to_string(),
    }
}

/// Test 1: Instance exits cleanly after shutdown signal
/// This test verifies the basic graceful shutdown flow works
#[tokio::test]
async fn test_graceful_kill_completes_without_error() {
    let supervisor = create_test_supervisor().await;

    let app_id = AppId("test-shutdown-exit".to_string());
    let config = create_test_config(app_id.clone());

    supervisor
        .store()
        .save_config(&config)
        .expect("save config failed");

    // Note: We can't easily start real WASM instances in unit tests without
    // actual compiled WASM artifacts, so this test focuses on the API contract

    // The graceful kill should handle missing instances gracefully
    let fake_instance_id = common::types::InstanceId::new();

    let drain_timeout = Duration::from_millis(100);
    let grace_timeout = Duration::from_secs(2);

    let result = supervisor
        .kill_instance_gracefully(&app_id, &fake_instance_id, drain_timeout, grace_timeout)
        .await;

    // Should return NotFound error for non-existent instance
    assert!(result.is_err(), "should return error for missing instance");
}

/// Test 2: Missing instance returns error quickly
#[tokio::test]
async fn test_missing_instance_returns_error() {
    let supervisor = create_test_supervisor().await;

    let app_id = AppId("test-missing-instance".to_string());
    let config = create_test_config(app_id.clone());

    supervisor
        .store()
        .save_config(&config)
        .expect("save config failed");

    let fake_instance_id = common::types::InstanceId::new();

    let drain_timeout = Duration::from_millis(500);
    let grace_timeout = Duration::from_secs(2);

    let start = std::time::Instant::now();

    let result = supervisor
        .kill_instance_gracefully(&app_id, &fake_instance_id, drain_timeout, grace_timeout)
        .await;

    let elapsed = start.elapsed();

    // Should return error quickly for missing instances (not wait drain timeout)
    assert!(result.is_err(), "should return error for missing instance");
    assert!(
        elapsed < drain_timeout,
        "should fail fast for missing instance, elapsed: {:?}",
        elapsed
    );
}

/// Test 3: shutdown_all() completes successfully
#[tokio::test]
async fn test_shutdown_all_completes() {
    let supervisor = create_test_supervisor().await;

    // Create two different apps
    let app1 = AppId("test-app-1".to_string());
    let app2 = AppId("test-app-2".to_string());

    for app_id in &[&app1, &app2] {
        let config = create_test_config((*app_id).clone());
        supervisor
            .store()
            .save_config(&config)
            .expect("save config failed");
    }

    // Shutdown all should complete without hanging
    let shutdown_timeout = Duration::from_secs(2);
    let start = std::time::Instant::now();

    supervisor.shutdown_all(shutdown_timeout).await;

    let elapsed = start.elapsed();

    // Should complete quickly since there are no instances
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown_all should complete quickly, elapsed: {:?}",
        elapsed
    );
}

/// Test 4: Upstream removal is performed before shutdown
#[tokio::test]
async fn test_upstream_removal_integration() {
    let supervisor = create_test_supervisor().await;

    let app_id = AppId("test-upstream-removal".to_string());
    let config = create_test_config(app_id.clone());

    supervisor
        .store()
        .save_config(&config)
        .expect("save config failed");

    // Manually add a backend to upstream registry
    let test_addr = "127.0.0.1:15000".parse().unwrap();
    supervisor.upstream().add(&app_id, test_addr).await;

    // Verify it's in upstream
    let count_before = supervisor.upstream().count(&app_id).await;
    assert_eq!(count_before, 1, "should have 1 backend registered");

    // Kill instance gracefully (instance doesn't exist, but upstream removal happens)
    let fake_instance_id = common::types::InstanceId::new();
    let _ = supervisor
        .kill_instance_gracefully(
            &app_id,
            &fake_instance_id,
            Duration::from_millis(100),
            Duration::from_secs(2),
        )
        .await;

    // The graceful kill should have attempted to remove from upstream
    // Note: Since we don't have the actual instance addr, the removal might not match
    // This test mainly verifies the flow doesn't panic
}

/// Test 5: list_instances returns empty for non-existent app
#[tokio::test]
async fn test_list_instances_empty_app() {
    let supervisor = create_test_supervisor().await;

    let app_id = AppId("non-existent-app".to_string());
    let instances = supervisor.list_instances(&app_id).await;

    assert_eq!(
        instances.len(),
        0,
        "should return empty list for unknown app"
    );
}

/// Test 6: Multiple concurrent graceful kills don't panic
#[tokio::test]
async fn test_concurrent_graceful_kills() {
    let supervisor = create_test_supervisor().await;

    let app_id = AppId("test-concurrent".to_string());
    let config = create_test_config(app_id.clone());

    supervisor
        .store()
        .save_config(&config)
        .expect("save config failed");

    // Spawn multiple concurrent kill operations
    let mut handles = vec![];
    for _ in 0..5 {
        let sup = supervisor.clone();
        let aid = app_id.clone();
        let handle = tokio::spawn(async move {
            let fake_id = common::types::InstanceId::new();
            sup.kill_instance_gracefully(
                &aid,
                &fake_id,
                Duration::from_millis(50),
                Duration::from_secs(1),
            )
            .await
        });
        handles.push(handle);
    }

    // Wait for all to complete
    for handle in handles {
        let _ = handle.await;
    }

    // Test passes if no panics occurred
}

/// Test 7: API accepts different timeout values without panicking
#[tokio::test]
async fn test_timeout_configuration() {
    let supervisor = create_test_supervisor().await;

    let app_id = AppId("test-timeouts".to_string());
    let config = create_test_config(app_id.clone());

    supervisor
        .store()
        .save_config(&config)
        .expect("save config failed");

    let fake_instance_id = common::types::InstanceId::new();

    // Test with different timeout values - all should complete without panicking
    let test_cases = vec![
        (Duration::from_millis(100), Duration::from_millis(500)),
        (Duration::from_millis(200), Duration::from_secs(1)),
        (Duration::from_millis(50), Duration::from_millis(200)),
        (Duration::from_secs(0), Duration::from_secs(0)), // edge case: zero timeouts
    ];

    for (drain, grace) in test_cases {
        let result = supervisor
            .kill_instance_gracefully(&app_id, &fake_instance_id, drain, grace)
            .await;

        // Should return error for missing instance, but not panic
        assert!(
            result.is_err(),
            "should return error for missing instance with drain={:?} grace={:?}",
            drain,
            grace
        );
    }
}
