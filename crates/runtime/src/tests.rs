use crate::WasmRuntime;
use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
use std::sync::Arc;
use std::thread;

fn base_config() -> AppConfig {
    AppConfig {
        id: AppId::new("test-app", "v1"),
        fuel_quota: FuelQuota(10_000),
        memory_limit: MemoryPages(5), // 5 pages = 320 KB
        env_vars: vec![],
        port: 8080,
        extended_limits: None,
    }
}

#[test]
fn test_runtime_initialization() {
    let runtime = WasmRuntime::new();
    assert!(Arc::strong_count(&runtime.engine) >= 1);
}

#[test]
fn test_compile_and_run_minimal() {
    let runtime = WasmRuntime::new();
    let wasm_bytes = wat::parse_str(
        r#"
        (component
            (core module $m
                (memory (export "memory") 1)
                (func (export "run")
                    nop
                )
            )
            (core instance $i (instantiate $m))
            (func (export "run") (canon lift (core func $i "run")))
        )
        "#,
    )
    .unwrap();

    // 1. Compile
    let artifact = runtime.compile(&wasm_bytes).expect("Compilation failed");
    assert!(!artifact.is_empty());

    // 2. Deserialize / Prepare
    let config = base_config();
    let prepared = runtime
        .prepare(&artifact, config.clone())
        .expect("Failed to prepare module");

    // 3. Spawn and Run
    let mut instance = prepared.spawn_instance(vec![], 8080).expect("Spawn failed");
    let stats = instance.run();

    assert!(
        stats.trap.is_none(),
        "Module trapped unexpectedly: {:?}",
        stats.trap
    );
    assert!(stats.fuel_consumed > 0, "Fuel should be consumed");
    assert!(stats.ram_bytes > 0, "Memory should be allocated");
}

#[test]
fn test_fuel_exhaustion_trap() {
    let runtime = WasmRuntime::new();
    // Infinite loop
    let wasm_bytes = wat::parse_str(
        r#"
        (component
            (core module $m
                (memory (export "memory") 1)
                (func (export "run")
                    (loop $my_loop
                        br $my_loop
                    )
                )
            )
            (core instance $i (instantiate $m))
            (func (export "run") (canon lift (core func $i "run")))
        )
        "#,
    )
    .unwrap();

    let artifact = runtime.compile(&wasm_bytes).unwrap();
    let mut config = base_config();
    config.fuel_quota = FuelQuota(1000); // Small fuel quota

    let prepared = runtime.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 8080).unwrap();
    let stats = instance.run();

    assert!(
        stats.trap.is_some(),
        "Infinite loop should trap due to out of fuel"
    );
    assert_eq!(stats.fuel_consumed, 1000);
}

#[test]
fn test_zero_fuel_immediate_trap() {
    let runtime = WasmRuntime::new();
    let wasm_bytes = wat::parse_str(
        r#"
        (component
            (core module $m
                (memory (export "memory") 1)
                (func (export "run")
                    nop
                )
            )
            (core instance $i (instantiate $m))
            (func (export "run") (canon lift (core func $i "run")))
        )
        "#,
    )
    .unwrap();

    let artifact = runtime.compile(&wasm_bytes).unwrap();
    let mut config = base_config();
    config.fuel_quota = FuelQuota(0);

    let prepared = runtime.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 8080).unwrap();
    let stats = instance.run();

    assert!(stats.trap.is_some(), "Zero fuel should trap immediately");
}

#[test]
fn test_memory_limit_enforced() {
    let runtime = WasmRuntime::new();
    // Tries to grow memory by 10 pages.
    // If it succeeds, it triggers unreachable (trap).
    // If it fails (returns -1), it exits cleanly.
    let wasm_bytes = wat::parse_str(
        r#"
        (component
            (core module $m
                (memory (export "memory") 1)
                (func (export "run")
                    (local $res i32)
                    (local.set $res (memory.grow (i32.const 10)))
                    (if (i32.ne (local.get $res) (i32.const -1))
                        (then (unreachable))
                    )
                )
            )
            (core instance $i (instantiate $m))
            (func (export "run") (canon lift (core func $i "run")))
        )
        "#,
    )
    .unwrap();

    let artifact = runtime.compile(&wasm_bytes).unwrap();
    let mut config = base_config();
    config.memory_limit = MemoryPages(2); // Limit is 2 pages, growth of 10 should be rejected

    let prepared = runtime.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 8080).unwrap();
    let stats = instance.run();

    // No trap means the memory.grow returned -1!
    assert!(
        stats.trap.is_none(),
        "Oversized memory.grow should return -1, not crash or trap"
    );
}

#[test]
fn test_concurrency() {
    let runtime = WasmRuntime::new();
    let wasm_bytes = wat::parse_str(
        r#"
        (component
            (core module $m
                (memory (export "memory") 1)
                (func (export "run") nop)
            )
            (core instance $i (instantiate $m))
            (func (export "run") (canon lift (core func $i "run")))
        )
        "#,
    )
    .unwrap();

    let artifact = runtime.compile(&wasm_bytes).unwrap();

    let r1 = runtime.clone();
    let a1 = artifact.clone();
    let t1 = thread::spawn(move || {
        let prepared = r1.prepare(&a1, base_config()).unwrap();
        let mut inst = prepared.spawn_instance(vec![], 8081).unwrap();
        inst.run()
    });

    let r2 = runtime.clone();
    let a2 = artifact.clone();
    let t2 = thread::spawn(move || {
        let prepared = r2.prepare(&a2, base_config()).unwrap();
        let mut inst = prepared.spawn_instance(vec![], 8082).unwrap();
        inst.run()
    });

    let s1 = t1.join().unwrap();
    let s2 = t2.join().unwrap();

    assert!(s1.trap.is_none());
    assert!(s2.trap.is_none());
}

#[test]
fn test_io_resource_tracker_enforcement() {
    use crate::limits::IoResourceTracker;
    use common::types::ExtendedLimits;

    let limits = ExtendedLimits {
        max_open_fds: 2,
        max_fs_write_bytes: 100,
        max_net_egress_bytes: 200,
        max_outbound_connections: 1,
    };
    let mut tracker = IoResourceTracker::new(limits);

    // 1. max_open_fds
    assert!(tracker.track_fd_open().is_ok());
    assert!(tracker.track_fd_open().is_ok());
    assert!(
        tracker.track_fd_open().is_err(),
        "Opening beyond max_open_fds should return error"
    );

    // fd_close decrements
    tracker.track_fd_close();
    assert!(
        tracker.track_fd_open().is_ok(),
        "Closing an fd should allow opening a new one"
    );

    // 2. max_fs_write_bytes
    assert!(tracker.track_fs_write(50).is_ok());
    assert!(tracker.track_fs_write(50).is_ok());
    assert!(
        tracker.track_fs_write(1).is_err(),
        "Writing beyond max_fs_write_bytes should return error"
    );

    // 3. max_net_egress_bytes
    assert!(tracker.track_net_egress(150).is_ok());
    assert!(tracker.track_net_egress(50).is_ok());
    assert!(
        tracker.track_net_egress(1).is_err(),
        "Sending beyond max_net_egress_bytes should return error"
    );

    // 4. max_outbound_connections
    assert!(tracker.track_outbound_connect().is_ok());
    assert!(
        tracker.track_outbound_connect().is_err(),
        "Connecting beyond max_outbound_connections should return error"
    );
}

#[test]
fn test_extended_limits_config_merge() {
    use common::types::{ExtendedLimits, ExtendedLimitsConfig};

    let config = ExtendedLimitsConfig {
        max_open_fds: Some(10),
        max_fs_write_bytes: None,
        max_net_egress_bytes: Some(500),
        max_outbound_connections: None,
    };

    let limits = config.to_limits();
    let defaults = ExtendedLimits::default();

    assert_eq!(limits.max_open_fds, 10, "User override should apply");
    assert_eq!(
        limits.max_fs_write_bytes, defaults.max_fs_write_bytes,
        "Missing field should use default"
    );
    assert_eq!(
        limits.max_net_egress_bytes, 500,
        "User override should apply"
    );
    assert_eq!(
        limits.max_outbound_connections, defaults.max_outbound_connections,
        "Missing field should use default"
    );
}

#[test]
fn test_default_extended_limits_applied() {
    // AppConfig without extended_limits uses default
    let config = base_config();
    assert!(config.extended_limits.is_none());

    let runtime = WasmRuntime::new();
    let wasm_bytes = wat::parse_str(
        r#"
        (component
            (core module $m
                (memory (export "memory") 1)
                (func (export "run") nop)
            )
            (core instance $i (instantiate $m))
            (func (export "run") (canon lift (core func $i "run")))
        )
        "#,
    )
    .unwrap();

    let artifact = runtime.compile(&wasm_bytes).unwrap();
    let prepared = runtime.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 8080).unwrap();
    let stats = instance.run();

    assert!(stats.trap.is_none());

    // Defaults should be populated in the execution stats backing tracker
    assert_eq!(stats.io_stats.open_fds_peak, 0);
    assert_eq!(stats.io_stats.fs_bytes_written, 0);
    assert_eq!(stats.io_stats.net_egress_bytes, 0);
    assert_eq!(stats.io_stats.outbound_connections, 0);
}
