use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
use runtime::WasmRuntime;
use std::path::PathBuf;

/// Test that the runtime can be instantiated.
#[test]
fn test_runtime_creation() {
    let _rt = WasmRuntime::new().expect("Failed to create WasmRuntime");
    // Successfully created
}

/// Test that default config can be created.
#[test]
fn test_default_config() {
    let config = AppConfig::default_for(AppId::new("test", "v1"));
    assert_eq!(config.id, AppId::new("test", "v1"));
    assert_eq!(config.fuel_quota.0, 500_000_000);
    assert_eq!(config.memory_limit.0, 2048);
    assert_eq!(config.max_instances, 10);
}

fn find_hello_axum_component_path() -> Option<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let candidates = [
        workspace_root.join("target/wasm32-wasip2/release/hello-axum.wasm"),
        workspace_root.join("target/wasm32-wasip2/release/hello_axum.wasm"),
    ];

    candidates.into_iter().find(|path| path.exists())
}

/// NOTE: The following tests require actual WASI Preview 2 component binaries.
/// The WAT component syntax is complex and requires proper tooling.
/// For full integration tests, we use the real hello-axum.wasm binary in E2E tests.
///
/// The runtime tests below are designed to work with real compiled components.
/// To run them, you need to:
/// 1. Build apps/hello-axum: `cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release`
/// 2. The E2E tests will use these real binaries
///
/// For now, we verify that the runtime infrastructure is correctly set up.

#[test]
fn test_compile_real_component() {
    let Some(wasm_path) = find_hello_axum_component_path() else {
        eprintln!("Skipping: hello-axum component binary not found");
        eprintln!("Build it with: cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release");
        return;
    };

    let wasm_bytes = std::fs::read(&wasm_path).expect("failed to read wasm file");
    let rt = WasmRuntime::new().expect("Failed to create WasmRuntime");

    let artifact = rt.compile(&wasm_bytes).expect("failed to compile");
    assert!(!artifact.is_empty(), "artifact should not be empty");

    let config = AppConfig::default_for(AppId::new("hello-axum", "v1"));
    let prepared = rt.prepare(&artifact, config).expect("failed to prepare");

    // Verify we can spawn an instance
    let instance = prepared
        .spawn_instance(vec![], 8080, None)
        .expect("failed to spawn");

    // We don't run it here because it's an HTTP server that would block
    drop(instance);
}

#[test]
fn test_artifact_roundtrip_with_real_component() {
    let Some(wasm_path) = find_hello_axum_component_path() else {
        eprintln!("Skipping: hello-axum component binary not found");
        return;
    };

    let wasm_bytes = std::fs::read(&wasm_path).expect("failed to read wasm file");
    let rt = WasmRuntime::new().expect("Failed to create WasmRuntime");

    // Compile
    let artifact = rt.compile(&wasm_bytes).expect("failed to compile");
    assert!(!artifact.is_empty());

    // Save artifact (simulating storage)
    let artifact_copy = artifact.clone();

    // Load artifact and prepare
    let config = AppConfig::default_for(AppId::new("test", "v1"));
    let prepared = rt
        .prepare(&artifact_copy, config)
        .expect("failed to prepare from artifact");

    // Verify we can spawn
    let _instance = prepared
        .spawn_instance(vec![], 8080, None)
        .expect("failed to spawn");
}

#[test]
fn test_multiple_instances_with_real_component() {
    let Some(wasm_path) = find_hello_axum_component_path() else {
        eprintln!("Skipping: hello-axum component binary not found");
        return;
    };

    let wasm_bytes = std::fs::read(&wasm_path).expect("failed to read wasm file");
    let rt = WasmRuntime::new().expect("Failed to create WasmRuntime");
    let artifact = rt.compile(&wasm_bytes).expect("failed to compile");

    let config = AppConfig {
        id: AppId::new("multi", "v1"),
        fuel_quota: FuelQuota(500_000_000),
        memory_limit: MemoryPages(2048),
        max_instances: 5,
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
        placement: common::types::PlacementPolicy::EveryNode,
        local_dependencies: Vec::new(),
    };

    let prepared = rt.prepare(&artifact, config).expect("failed to prepare");

    // Spawn multiple instances
    let _inst1 = prepared
        .spawn_instance(vec![], 8081, None)
        .expect("spawn 1 failed");
    let _inst2 = prepared
        .spawn_instance(vec![], 8082, None)
        .expect("spawn 2 failed");
    let _inst3 = prepared
        .spawn_instance(vec![], 8083, None)
        .expect("spawn 3 failed");

    // All instances created successfully
}

/// Test memory limit configuration.
#[test]
fn test_memory_limit_config() {
    let config = AppConfig {
        id: AppId::new("memtest", "v1"),
        fuel_quota: FuelQuota(10_000_000),
        memory_limit: MemoryPages(10), // Very small: 640 KB
        max_instances: 1,
        idle_timeout_secs: 60,
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
        placement: common::types::PlacementPolicy::EveryNode,
        local_dependencies: Vec::new(),
    };

    assert_eq!(config.memory_limit.0, 10);
    assert_eq!(config.memory_limit.to_bytes(), 10 * 64 * 1024);
}

/// Test fuel quota configuration.
#[test]
fn test_fuel_quota_config() {
    let high_fuel = AppConfig {
        id: AppId::new("high", "v1"),
        fuel_quota: FuelQuota(10_000_000),
        memory_limit: MemoryPages(256),
        max_instances: 1,
        idle_timeout_secs: 60,
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
        placement: common::types::PlacementPolicy::EveryNode,
        local_dependencies: Vec::new(),
    };

    let low_fuel = AppConfig {
        id: AppId::new("low", "v1"),
        fuel_quota: FuelQuota(100),
        memory_limit: MemoryPages(256),
        max_instances: 1,
        idle_timeout_secs: 60,
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
        placement: common::types::PlacementPolicy::EveryNode,
        local_dependencies: Vec::new(),
    };

    assert_eq!(high_fuel.fuel_quota.0, 10_000_000);
    assert_eq!(low_fuel.fuel_quota.0, 100);
}
