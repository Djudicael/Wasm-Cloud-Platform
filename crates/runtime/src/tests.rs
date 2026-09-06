use crate::{
    current_policy_boundary,
    executor::{
        compose_socket_addr_check, top_level_entry_point_candidates, ComponentExecutionModel,
        SocketAddrUse, SocketPolicyCheck,
    },
    policy_tracker::PolicyEnforcer,
    PolicyEnforcementLayer, WasmRuntime,
};
use common::{
    policy::{
        FilesystemPolicy, FilesystemPolicyConfig, InstancePolicy, NetworkPolicy, PolicyConfig,
        PolicyProfile,
    },
    types::{AppConfig, AppId, FuelQuota, MemoryPages},
};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use tempfile::TempDir;

fn base_config() -> AppConfig {
    let mut config = AppConfig::default_for(AppId::new("test-app", "v1"));
    config.fuel_quota = FuelQuota(10_000);
    config.memory_limit = MemoryPages(5); // 5 pages = 320 KB
    config.wasm_bind_port = 8080;
    config
}

fn base_instance_policy() -> InstancePolicy {
    InstancePolicy {
        network: NetworkPolicy {
            allowed_bind_ports: vec![8080],
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    }
}

fn compile_and_run_component(component_wat: &str) -> crate::executor::ExecutionStats {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    let wasm_bytes = wat::parse_str(component_wat).expect("failed to parse component WAT");
    let artifact = runtime.compile(&wasm_bytes).expect("Compilation failed");
    let prepared = runtime
        .prepare(&artifact, base_config())
        .expect("Failed to prepare module");
    let mut instance = prepared
        .spawn_instance(vec![], 8080, None)
        .expect("Spawn failed");
    instance.run()
}

fn no_op_component_with_top_level_export(component_export_name: &str) -> String {
    let core_export_name = "entry_impl";
    format!(
        r#"
        (component
            (core module $m
                (memory (export "memory") 1)
                (func (export "{core_export_name}")
                    nop
                )
            )
            (core instance $i (instantiate $m))
            (func (export "{component_export_name}") (canon lift (core func $i "{core_export_name}")))
        )
        "#
    )
}

fn no_op_component_with_wasi_cli_run_interface() -> &'static str {
    r#"
    (component
        (core module $m
            (memory (export "memory") 1)
            (func (export "run")
                nop
            )
        )
        (core instance $i (instantiate $m))
        (type $run-func (func))
        (func $run (type $run-func) (canon lift (core func $i "run")))
        (instance $cli-run
            (export "run" (func $run))
        )
        (export "wasi:cli/run@0.2.6" (instance $cli-run))
    )
    "#
}

fn no_op_component_with_wasi_http_incoming_handler_interface() -> &'static str {
    r#"
    (component
        (core module $m
            (memory (export "memory") 1)
            (func (export "handle")
                nop
            )
        )
        (core instance $i (instantiate $m))
        (type $handle-func (func))
        (func $handle (type $handle-func) (canon lift (core func $i "handle")))
        (instance $incoming-handler
            (export "handle" (func $handle))
        )
        (export "wasi:http/incoming-handler@0.2.3" (instance $incoming-handler))
    )
    "#
}

fn base_config_with_policy(policy: PolicyConfig) -> AppConfig {
    let mut config = base_config();
    config.policy = Some(policy);
    config
}

fn find_hello_axum_component_path() -> Option<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let candidates = [
        target_root.join("wasm32-wasip2/release/hello-axum.wasm"),
        target_root.join("wasm32-wasip2/release/hello_axum.wasm"),
    ];

    candidates.into_iter().find(|path| path.exists())
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

    let Some(wasm_path) = find_hello_axum_component_path() else {
        eprintln!(
            "Skipping hello-axum export inspection: build apps/hello-axum for wasm32-wasip2 first"
        );
        return;
    };

    let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read WASM file");

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
    crate::limits::configure_store(&mut store, FuelQuota(500_000_000))
        .expect("Failed to configure test store limits");

    let instance = linker
        .instantiate(&mut store, &component)
        .expect("Failed to instantiate");

    let interface_idx = instance
        .get_export_index(&mut store, None, "wasi:cli/run@0.2.6")
        .expect("hello-axum does not export wasi:cli/run@0.2.6");
    let function_idx = instance
        .get_export_index(&mut store, Some(&interface_idx), "run")
        .expect("wasi:cli/run@0.2.6 does not export run");
    assert!(
        instance.get_func(&mut store, function_idx).is_some(),
        "wasi:cli/run@0.2.6#run is not a function"
    );
}

#[test]
fn test_runtime_initialization() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    assert!(Arc::strong_count(&runtime.engine) >= 1);
}

#[test]
fn test_policy_boundary_declares_runtime_socket_gate_as_authoritative_for_tcp() {
    let boundary = current_policy_boundary();

    let tcp_bind = boundary
        .iter()
        .find(|cap| cap.capability == "tcp_bind")
        .expect("tcp_bind capability should be declared");
    assert_eq!(
        tcp_bind.primary_layer,
        PolicyEnforcementLayer::WasmtimeSocketAddrCheck
    );
    assert!(tcp_bind.authoritative_enforcement);
    assert!(tcp_bind.authoritative_counters);

    let tcp_connect = boundary
        .iter()
        .find(|cap| cap.capability == "tcp_connect")
        .expect("tcp_connect capability should be declared");
    assert_eq!(
        tcp_connect.primary_layer,
        PolicyEnforcementLayer::WasmtimeSocketAddrCheck
    );
    assert!(tcp_connect.authoritative_enforcement);
    assert!(tcp_connect.authoritative_counters);
}

#[test]
fn test_policy_boundary_declares_remaining_non_authoritative_gaps_explicitly() {
    let boundary = current_policy_boundary();

    let dns = boundary
        .iter()
        .find(|cap| cap.capability == "dns_lookup")
        .expect("dns_lookup capability should be declared");
    assert_eq!(
        dns.primary_layer,
        PolicyEnforcementLayer::WasmtimeNetworkToggle
    );
    assert!(dns.authoritative_enforcement);
    assert!(!dns.authoritative_counters);

    let fs_write = boundary
        .iter()
        .find(|cap| cap.capability == "filesystem_write_bytes")
        .expect("filesystem_write_bytes capability should be declared");
    assert_eq!(fs_write.primary_layer, PolicyEnforcementLayer::ExternalEbpf);
    assert!(!fs_write.authoritative_enforcement);
    assert!(!fs_write.authoritative_counters);

    let net_egress = boundary
        .iter()
        .find(|cap| cap.capability == "network_egress_bytes")
        .expect("network_egress_bytes capability should be declared");
    assert_eq!(
        net_egress.primary_layer,
        PolicyEnforcementLayer::ExternalEbpf
    );
    assert!(!net_egress.authoritative_enforcement);
    assert!(!net_egress.authoritative_counters);

    let resource_limits = boundary
        .iter()
        .find(|cap| cap.capability == "memory_and_table_growth")
        .expect("memory_and_table_growth capability should be declared");
    assert_eq!(
        resource_limits.primary_layer,
        PolicyEnforcementLayer::WasmtimeResourceLimiter
    );
    assert!(resource_limits.authoritative_enforcement);
    assert!(resource_limits.authoritative_counters);
}

#[test]
fn test_runtime_initialization_with_code_cache_directory() {
    let temp_dir = TempDir::new().unwrap();
    let runtime_cfg = common::config::RuntimeSection {
        cache_directory: Some(
            temp_dir
                .path()
                .join("wasmtime-cache")
                .to_string_lossy()
                .to_string(),
        ),
        ..Default::default()
    };

    let runtime = WasmRuntime::new_with_runtime_config(Some(&runtime_cfg))
        .expect("Failed to create WasmRuntime with cache directory");
    assert!(Arc::strong_count(&runtime.engine) >= 1);
    assert!(temp_dir.path().join("wasmtime-cache").exists());
    assert!(temp_dir
        .path()
        .join("wasmtime-cache")
        .join("wasmtime-cache-config.toml")
        .exists());
}

#[test]
fn test_runtime_initialization_with_pooling_allocator() {
    let runtime_cfg = common::config::RuntimeSection {
        pooling_allocator: true,
        pooling_total_component_instances: 64,
        pooling_max_core_instances_per_component: Some(8),
        pooling_max_memories_per_component: Some(4),
        pooling_max_tables_per_component: Some(4),
        ..Default::default()
    };

    let runtime = WasmRuntime::new_with_runtime_config(Some(&runtime_cfg))
        .expect("Failed to create WasmRuntime with pooling allocator");
    assert!(Arc::strong_count(&runtime.engine) >= 1);
}

#[test]
fn test_run_supports_wasi_cli_run_interface_export() {
    let stats = compile_and_run_component(no_op_component_with_wasi_cli_run_interface());
    assert!(
        stats.trap.is_none(),
        "wasi:cli/run interface export should execute successfully: {:?}",
        stats.trap
    );
}

#[test]
fn test_run_supports_top_level_run_fallback() {
    let stats = compile_and_run_component(&no_op_component_with_top_level_export("run"));
    assert!(
        stats.trap.is_none(),
        "top-level run fallback should execute successfully: {:?}",
        stats.trap
    );
}

#[test]
fn test_prepare_detects_wasi_http_incoming_handler_components() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    let wasm_bytes = wat::parse_str(no_op_component_with_wasi_http_incoming_handler_interface())
        .expect("failed to parse component WAT");
    let artifact = runtime.compile(&wasm_bytes).expect("Compilation failed");
    let prepared = runtime
        .prepare(&artifact, base_config())
        .expect("Failed to prepare module");
    assert_eq!(
        prepared.execution_model(),
        ComponentExecutionModel::WasiHttpIncomingHandler
    );
}

#[tokio::test]
async fn test_dedicated_http_executor_stays_on_registered_thread() {
    let (registered_thread_tx, registered_thread_rx) = tokio::sync::oneshot::channel();

    let task = super::executor::spawn_dedicated_current_thread(
        Some(Box::new(move || {
            let _ = registered_thread_tx.send(thread::current().id());
        })),
        || async {
            let before_yield = thread::current().id();
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            (before_yield, thread::current().id())
        },
        |err| panic!("failed to create dedicated runtime: {err}"),
    );

    let registered_thread = registered_thread_rx.await.unwrap();
    let (before_yield, after_yield) = task.await.unwrap();
    assert_eq!(registered_thread, before_yield);
    assert_eq!(before_yield, after_yield);
    assert_ne!(registered_thread, thread::current().id());
}

#[test]
fn test_top_level_entry_point_candidates_include_start_fallback() {
    assert_eq!(top_level_entry_point_candidates(), &["run", "_start"]);
}

#[test]
fn test_spawn_instance_preopens_allowed_filesystem_path() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config = base_config_with_policy(PolicyConfig {
        network: None,
        filesystem: Some(FilesystemPolicyConfig {
            max_open_fds: None,
            max_fs_write_bytes: None,
            max_fs_read_bytes: None,
            allow_file_create: Some(false),
            allow_file_delete: Some(false),
            allowed_paths: Some(vec![temp_dir.path().to_string_lossy().to_string()]),
        }),
    });

    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    let wasm_bytes = wat::parse_str(no_op_component_with_wasi_cli_run_interface()).unwrap();
    let artifact = runtime.compile(&wasm_bytes).unwrap();
    let prepared = runtime.prepare(&artifact, config).unwrap();

    let instance = prepared.spawn_instance(vec![], 8080, None);
    assert!(instance.is_ok(), "allowed preopen path should succeed");
}

#[test]
fn test_spawn_instance_fails_for_missing_allowed_filesystem_path() {
    let missing_path = "/definitely-missing-wcp-preopen-path".to_string();
    let config = base_config_with_policy(PolicyConfig {
        network: None,
        filesystem: Some(FilesystemPolicyConfig {
            max_open_fds: None,
            max_fs_write_bytes: None,
            max_fs_read_bytes: None,
            allow_file_create: Some(false),
            allow_file_delete: Some(false),
            allowed_paths: Some(vec![missing_path.clone()]),
        }),
    });

    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    let wasm_bytes = wat::parse_str(no_op_component_with_wasi_cli_run_interface()).unwrap();
    let artifact = runtime.compile(&wasm_bytes).unwrap();
    let err = match runtime.prepare(&artifact, config) {
        Ok(_) => panic!("missing preopen path should fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("failed to preopen allowed path"));
    assert!(err.to_string().contains(&missing_path));
}

#[test]
fn test_socket_policy_check_separates_bind_from_outbound_tcp() {
    let mut policy = base_instance_policy();
    policy.network.allow_outbound_tcp = false;
    policy.network.allow_inbound = true;

    let check = SocketPolicyCheck::from_instance_policy(&policy);

    assert!(check
        .check("127.0.0.1:8080".parse().unwrap(), SocketAddrUse::TcpBind)
        .is_ok());
    assert_eq!(
        check.check(
            "93.184.216.34:443".parse().unwrap(),
            SocketAddrUse::TcpConnect
        ),
        Err("outbound tcp disabled")
    );
}

#[test]
fn test_socket_policy_check_handles_wasmtime_48_socket_operations() {
    let policy = base_instance_policy();
    let check = SocketPolicyCheck::from_instance_policy(&policy);

    assert!(check
        .check("127.0.0.1:8080".parse().unwrap(), SocketAddrUse::TcpListen)
        .is_ok());
    assert!(check
        .check("127.0.0.1:49152".parse().unwrap(), SocketAddrUse::TcpAccept)
        .is_ok());

    let mut denied = policy.clone();
    denied.network.allow_inbound = false;
    let denied_check = SocketPolicyCheck::from_instance_policy(&denied);
    assert_eq!(
        denied_check.check("127.0.0.1:49152".parse().unwrap(), SocketAddrUse::TcpAccept),
        Err("inbound tcp accept disabled")
    );

    let mut udp = policy;
    udp.network.allow_outbound_udp = true;
    let udp_check = SocketPolicyCheck::from_instance_policy(&udp);
    assert!(udp_check
        .check("93.184.216.34:53".parse().unwrap(), SocketAddrUse::UdpSend)
        .is_ok());
    assert!(udp_check
        .check(
            "93.184.216.34:53".parse().unwrap(),
            SocketAddrUse::UdpReceive
        )
        .is_ok());
}

#[test]
fn test_socket_policy_check_applies_cidr_filters() {
    let mut policy = base_instance_policy();
    policy.network.allow_outbound_tcp = true;
    policy.network.allowed_cidrs = vec!["93.184.216.0/24".to_string()];
    policy.network.denied_cidrs = vec!["93.184.216.34/32".to_string()];

    let check = SocketPolicyCheck::from_instance_policy(&policy);

    assert!(check
        .check(
            "93.184.216.35:443".parse().unwrap(),
            SocketAddrUse::TcpConnect
        )
        .is_ok());
    assert_eq!(
        check.check(
            "93.184.216.34:443".parse().unwrap(),
            SocketAddrUse::TcpConnect
        ),
        Err("destination in denied_cidrs")
    );
    assert_eq!(
        check.check("10.0.0.1:443".parse().unwrap(), SocketAddrUse::TcpConnect),
        Err("destination not in allowed_cidrs")
    );
}

#[tokio::test]
async fn test_composed_socket_addr_check_denies_policy_before_extra_allow() {
    let mut policy = base_instance_policy();
    policy.network.allow_outbound_tcp = false;
    policy.network.allow_inbound = true;

    let composed = compose_socket_addr_check(
        SocketPolicyCheck::from_instance_policy(&policy),
        PolicyEnforcer::new(policy.clone()),
        Some(Arc::new(|_, _| Box::pin(async { true }))),
    );

    assert!(
        !(composed)(
            "93.184.216.34:443".parse().unwrap(),
            SocketAddrUse::TcpConnect,
        )
        .await
    );
    assert!((composed)("127.0.0.1:8080".parse().unwrap(), SocketAddrUse::TcpBind).await);
}

#[tokio::test]
async fn test_composed_socket_addr_check_records_policy_tracked_tcp_connects() {
    let mut policy = base_instance_policy();
    policy.network.allow_outbound_tcp = true;

    let enforcer = PolicyEnforcer::new(policy.clone());
    let composed = compose_socket_addr_check(
        SocketPolicyCheck::from_instance_policy(&policy),
        enforcer.clone(),
        Some(Arc::new(|_, _| Box::pin(async { true }))),
    );

    assert!(
        (composed)(
            "93.184.216.34:443".parse().unwrap(),
            SocketAddrUse::TcpConnect,
        )
        .await
    );
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_total
            .load(Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn test_composed_socket_addr_check_rolls_back_reserved_slot_when_extra_check_denies() {
    let mut policy = base_instance_policy();
    policy.network.allow_outbound_tcp = true;

    let enforcer = PolicyEnforcer::new(policy.clone());
    let composed = compose_socket_addr_check(
        SocketPolicyCheck::from_instance_policy(&policy),
        enforcer.clone(),
        Some(Arc::new(|_, _| Box::pin(async { false }))),
    );

    assert!(
        !(composed)(
            "93.184.216.34:443".parse().unwrap(),
            SocketAddrUse::TcpConnect,
        )
        .await
    );
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_total
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn test_composed_socket_addr_check_uses_policy_enforcer_bind_denial_counters() {
    let policy = PolicyProfile::BackgroundWorker
        .to_config()
        .resolve(8080)
        .unwrap();
    let enforcer = PolicyEnforcer::new(policy.clone());
    let composed = compose_socket_addr_check(
        SocketPolicyCheck::from_instance_policy(&policy),
        enforcer.clone(),
        None,
    );

    assert!(!(composed)("127.0.0.1:8080".parse().unwrap(), SocketAddrUse::TcpBind).await);
    assert_eq!(
        enforcer.counters.bind_denied_total.load(Ordering::Relaxed),
        1
    );
}

#[test]
fn test_static_site_profile_denies_outbound_tcp() {
    let policy = PolicyProfile::StaticSite.to_config().resolve(8080).unwrap();
    let check = SocketPolicyCheck::from_instance_policy(&policy);

    assert!(check
        .check("127.0.0.1:8080".parse().unwrap(), SocketAddrUse::TcpBind)
        .is_ok());
    assert_eq!(
        check.check(
            "93.184.216.34:443".parse().unwrap(),
            SocketAddrUse::TcpConnect
        ),
        Err("outbound tcp disabled")
    );
}

#[test]
fn test_background_worker_profile_denies_tcp_bind() {
    let policy = PolicyProfile::BackgroundWorker
        .to_config()
        .resolve(8080)
        .unwrap();
    let check = SocketPolicyCheck::from_instance_policy(&policy);

    assert_eq!(
        check.check("127.0.0.1:8080".parse().unwrap(), SocketAddrUse::TcpBind),
        Err("inbound tcp bind disabled")
    );
    assert!(check
        .check(
            "93.184.216.34:443".parse().unwrap(),
            SocketAddrUse::TcpConnect
        )
        .is_ok());
}

#[test]
fn test_execution_stats_export_authoritative_policy_counters() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    let wasm_bytes = wat::parse_str(no_op_component_with_wasi_cli_run_interface()).unwrap();
    let artifact = runtime.compile(&wasm_bytes).unwrap();
    let prepared = runtime.prepare(&artifact, base_config()).unwrap();

    let mut instance = prepared
        .spawn_instance(vec![], 8080, None)
        .expect("Spawn failed");
    let counters = instance.policy_counters();
    counters.open_fds.store(1, Ordering::Relaxed);
    counters.open_fds_peak.store(3, Ordering::Relaxed);
    counters.fs_write_bytes.store(4096, Ordering::Relaxed);
    counters.egress_bytes.store(2048, Ordering::Relaxed);
    counters
        .outbound_connections_total
        .store(2, Ordering::Relaxed);

    let stats = instance.run();
    assert!(stats.trap.is_none(), "run should succeed: {:?}", stats.trap);
    assert_eq!(stats.io_stats.open_fds_peak, 3);
    assert_eq!(stats.io_stats.fs_bytes_written, 4096);
    assert_eq!(stats.io_stats.net_egress_bytes, 2048);
    assert_eq!(stats.io_stats.outbound_connections, 2);
}

#[test]
fn test_running_instance_drop_resets_active_policy_counters() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
    let wasm_bytes = wat::parse_str(no_op_component_with_wasi_cli_run_interface()).unwrap();
    let artifact = runtime.compile(&wasm_bytes).unwrap();
    let prepared = runtime.prepare(&artifact, base_config()).unwrap();

    let counters = {
        let instance = prepared
            .spawn_instance(vec![], 8080, None)
            .expect("Spawn failed");
        let counters = instance.policy_counters();
        counters.open_fds.store(2, Ordering::Relaxed);
        counters.open_fds_peak.store(2, Ordering::Relaxed);
        counters
            .outbound_connections_active
            .store(1, Ordering::Relaxed);
        counters
    };

    assert_eq!(counters.open_fds.load(Ordering::Relaxed), 0);
    assert_eq!(
        counters.outbound_connections_active.load(Ordering::Relaxed),
        0
    );
    assert_eq!(counters.open_fds_peak.load(Ordering::Relaxed), 2);
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
    let mut instance = prepared
        .spawn_instance(vec![], 8080, None)
        .expect("Spawn failed");
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
#[cfg_attr(windows, ignore = "MSVC unwinding issue on traps")]
fn test_long_running_service_guest_is_still_bounded_by_fuel() {
    let runtime = WasmRuntime::new().expect("Failed to create WasmRuntime");
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
    config.fuel_quota = FuelQuota(10_000);

    let prepared = runtime.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 8080, None).unwrap();
    let stats = instance.run();

    assert!(
        stats.trap.is_some(),
        "long-running service guest should trap when its fuel is exhausted"
    );
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
        max_table_elements: Some(2048),
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
    assert_eq!(
        limits.max_table_elements, 2048,
        "User override should apply"
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

#[test]
fn test_default_extended_limits_include_table_cap() {
    let defaults = common::types::ExtendedLimits::default();
    assert_eq!(defaults.max_table_elements, 10_000);
}
