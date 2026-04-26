use crate::WasmRuntime;
use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
use std::sync::Arc;
use std::thread;

fn base_config() -> AppConfig {
    let mut config = AppConfig::default_for(AppId::new("test-app", "v1"));
    config.fuel_quota = FuelQuota(10_000);
    config.memory_limit = MemoryPages(5); // 5 pages = 320 KB
    config.wasm_bind_port = 8080;
    config
}

#[test]
fn test_list_hello_axum_exports() {
    use wasmtime::component::ResourceTable;
    use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

    struct TestStoreState {
        ctx: WasiCtx,
        table: ResourceTable,
    }

    impl WasiView for TestStoreState {
        fn ctx(&mut self) -> WasiCtxView<'_> {
            WasiCtxView {
                ctx: &mut self.ctx,
                table: &mut self.table,
            }
        }
    }

    let wasm_bytes = std::fs::read(
        "/mnt/d/dev/Wasm-Cloud-Platform/target/wasm32-wasip2/release/hello-axum.wasm",
    )
    .expect("Failed to read WASM file");

    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    let component = wasmtime::component::Component::from_binary(&runtime.engine, &wasm_bytes)
        .expect("Failed to parse component");

    let mut linker = wasmtime::component::Linker::new(&runtime.engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("Failed to add to linker");

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdout();
    builder.inherit_stderr();
    builder.inherit_network();
    builder.allow_tcp(true);
    let state = TestStoreState {
        ctx: builder.build(),
        table: ResourceTable::new(),
    };
    let mut store = wasmtime::Store::new(&runtime.engine, state);

    let instance = linker
        .instantiate(&mut store, &component)
        .expect("Failed to instantiate");

    // Try to find the wasi:cli/run@0.2.6 interface
    println!("\n=== Looking for wasi:cli/run@0.2.6 ===");
    let interface_idx = instance.get_export_index(&mut store, None, "wasi:cli/run@0.2.6");
    println!("Interface index: {:?}", interface_idx);

    if let Some(idx) = interface_idx {
        println!("\n=== Looking for run function inside interface ===");
        let func_idx = instance.get_export_index(&mut store, Some(&idx), "run");
        println!("Function index: {:?}", func_idx);

        // Unwrap the Option and use the index directly
        if let Some(func_export_idx) = func_idx {
            // Try to get the function using the index directly
            if let Some(func) = instance.get_func(&mut store, func_export_idx) {
                println!("Found run function! Type: {:?}", func.ty(&store));

                // Try typed call
                match func.typed::<(), (Result<(), ()>,)>(&store) {
                    Ok(typed) => {
                        println!("Typed call created successfully!");
                        match typed.call(&mut store, ()) {
                            Ok((result,)) => {
                                println!("Call succeeded, result: {:?}", result);
                            }
                            Err(e) => {
                                println!("Call failed: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        println!("Failed to create typed call: {}", e);
                    }
                }
            } else {
                println!("Could not get func from func_idx");
            }
        }
    } else {
        println!("Could not find wasi:cli/run@0.2.6 interface");
    }
}

#[test]
fn test_runtime_initialization() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    assert!(Arc::strong_count(&runtime.engine) >= 1);
}

#[test]
fn test_compile_and_run_minimal() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
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
    let mut instance = prepared.spawn_instance(vec![], 8080, None).expect("Spawn failed");
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
#[cfg_attr(windows, ignore = "MSVC unwinding issue on traps")]
fn test_fuel_exhaustion_trap() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
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
    let mut instance = prepared.spawn_instance(vec![], 8080, None).unwrap();
    let stats = instance.run();

    assert!(
        stats.trap.is_some(),
        "Infinite loop should trap due to out of fuel"
    );
    assert_eq!(stats.fuel_consumed, 1000);
}

#[test]
#[cfg_attr(windows, ignore = "MSVC unwinding issue on traps")]
fn test_zero_fuel_immediate_trap() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
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
    config.fuel_quota = FuelQuota(1); // MSVC Wasmtime panics on absolute 0.

    let prepared = runtime.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 8080, None).unwrap();
    let stats = instance.run();

    assert!(stats.trap.is_some(), "Zero fuel should trap immediately");
}

#[test]
fn test_memory_limit_enforced() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
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
    let mut instance = prepared.spawn_instance(vec![], 8080, None).unwrap();
    let stats = instance.run();

    // No trap means the memory.grow returned -1!
    assert!(
        stats.trap.is_none(),
        "Oversized memory.grow should return -1, not crash or trap"
    );
}

#[test]
fn test_concurrency() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
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
        let mut inst = prepared.spawn_instance(vec![], 8081, None).unwrap();
        inst.run()
    });

    let r2 = runtime.clone();
    let a2 = artifact.clone();
    let t2 = thread::spawn(move || {
        let prepared = r2.prepare(&a2, base_config()).unwrap();
        let mut inst = prepared.spawn_instance(vec![], 8082, None).unwrap();
        inst.run()
    });

    let s1 = t1.join().unwrap();
    let s2 = t2.join().unwrap();

    assert!(s1.trap.is_none());
    assert!(s2.trap.is_none());
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

    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
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
    let mut instance = prepared.spawn_instance(vec![], 8080, None).unwrap();
    let stats = instance.run();

    assert!(stats.trap.is_none());

    // Defaults should be populated in the execution stats backing tracker
    assert_eq!(stats.io_stats.open_fds_peak, 0);
    assert_eq!(stats.io_stats.fs_bytes_written, 0);
    assert_eq!(stats.io_stats.net_egress_bytes, 0);
    assert_eq!(stats.io_stats.outbound_connections, 0);
}
