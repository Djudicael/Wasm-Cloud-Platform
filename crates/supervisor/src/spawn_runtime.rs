//! Instance spawn flow: config checks, namespace-aware environment setup,
//! runtime launch, and registration into routing and local state.

use crate::{
    instance::{BillingInfo, ManagedInstance},
    is_instance_bind_allowed,
    network_interceptor::{ConnectDecision, NetworkInterceptor},
    pool::InstancePool,
    Supervisor,
};
use common::{
    error::PlatformError,
    types::{AppConfig, AppId, InstanceId, InstanceState},
};
use messaging::events::Event;
use runtime::{
    executor::{ComponentExecutionModel, ExecutionStats, SocketAddrCheckFn, SocketAddrUse},
    limits::IoStats,
};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::info;

impl Supervisor {
    /// Enforce node-local ceilings before an app can be admitted to a pool.
    pub fn check_resource_limits(&self, config: &AppConfig) -> Result<(), PlatformError> {
        // Maximum fuel quota: 10 billion units (prevents absurdly long compute)
        if config.fuel_quota.0 > 10_000_000_000 {
            return Err(PlatformError::runtime(
                "fuel_quota exceeds maximum allowed (10B units)",
            ));
        }

        // Maximum memory: 512 MB (8192 pages)
        if config.memory_limit.0 > 8192 {
            return Err(PlatformError::runtime(
                "memory_limit exceeds maximum allowed (512 MB)",
            ));
        }

        // Maximum concurrent instances per app per node: 100
        if config.max_instances > 100 {
            return Err(PlatformError::runtime(
                "max_instances exceeds node limit (100)",
            ));
        }

        let declared_pool_bytes = config
            .memory_limit
            .to_bytes()
            .checked_mul(u64::from(config.max_instances))
            .ok_or_else(|| PlatformError::runtime("declared application memory pool overflows"))?;
        if declared_pool_bytes > self.node_memory_budget_bytes {
            return Err(PlatformError::runtime(format!(
                "declared application memory pool ({} bytes) exceeds node budget ({} bytes)",
                declared_pool_bytes, self.node_memory_budget_bytes
            )));
        }

        Ok(())
    }

    /// Ensure at least one instance is available for the given app.
    pub async fn ensure_instance(&self, app_id: &AppId) -> Result<SocketAddr, PlatformError> {
        let existing = {
            let pools = self.pools.read().await;
            pools
                .get(&app_id.0)
                .and_then(|pool| pool.ready_addrs().first().copied())
        };

        if let Some(addr) = existing {
            return Ok(addr);
        }

        self.spawn(app_id).await
    }

    /// Spawn a new instance for the given app.
    /// Returns the SocketAddr where the instance is listening.
    pub async fn spawn(&self, app_id: &AppId) -> Result<SocketAddr, PlatformError> {
        if self.store.is_undeployed(&app_id.0)? {
            return Err(PlatformError::AppNotFound(format!(
                "{} is undeployed",
                app_id.0
            )));
        }

        let (config, prepared) = {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&app_id.0) {
                (pool.config.clone(), pool.prepared.clone())
            } else {
                let config = self
                    .store
                    .load_config(app_id)?
                    .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

                // 1. Load or compile the artifact
                let artifact = self.store.load_artifact(app_id)?.ok_or_else(|| {
                    PlatformError::AppNotFound(format!("no artifact for {}", app_id.0))
                })?;

                // 2. Prepare the module
                let prepared = Arc::new(self.runtime.prepare(&artifact, config.clone())?);

                (config, prepared)
            }
        };

        // Re-check at the execution boundary so legacy persisted state or a
        // future ingestion path cannot bypass admission policy.
        self.check_resource_limits(&config)?;

        for dependency in &config.local_dependencies {
            let dependency_is_available = !self.store.is_undeployed(&dependency.0)?
                && self.store.load_config(dependency)?.is_some()
                && self.store.load_artifact(dependency)?.is_some();
            if !dependency_is_available {
                return Err(PlatformError::AppNotFound(format!(
                    "required node-local dependency {} for {} is unavailable",
                    dependency.0, app_id.0
                )));
            }
        }

        // Build a qualified AppId that includes the namespace from config.
        let version = app_id.bare_name().split(':').nth(1).unwrap_or("v1");
        let qualified_app_id =
            AppId::new_namespaced(&config.namespace, app_id.bare_app_name(), version);

        tracing::info!(
            app_id = %app_id.0,
            config_namespace = %config.namespace,
            qualified_app_id = %qualified_app_id.0,
            "[SPAWN] Building qualified AppId"
        );

        // 3. Allocate a host port
        let host_port = self.port_alloc.allocate()?;
        let addr = self.port_alloc.socket_addr(host_port);

        // 4. Resolve env vars - note: we pass host_port, not wasm_bind_port
        let mut env_vars = (self.env_resolver)(&config, host_port);

        // 4b. Inject service discovery env vars for other running apps in the same namespace
        let target_namespace = qualified_app_id.namespace();
        let ns_services = self
            .service_registry
            .get_namespace_services(target_namespace)
            .await;

        tracing::info!(
            target_namespace = %target_namespace,
            ns_services_count = ns_services.len(),
            "[SPAWN] Namespace services query"
        );

        for (bare_app_name, addrs) in &ns_services {
            if bare_app_name == app_id.bare_app_name() {
                continue;
            }
            if let Some(addr) = addrs.first() {
                let key = format!(
                    "{}_SERVICE_URL",
                    bare_app_name.to_uppercase().replace('-', "_")
                );

                let unqualified = format!("{}:v1", bare_app_name);
                let qualified =
                    AppId::new_namespaced(qualified_app_id.namespace(), bare_app_name, "v1").0;
                let gateway_config = self
                    .store
                    .load_gateway_config(&unqualified)
                    .ok()
                    .flatten()
                    .or_else(|| self.store.load_gateway_config(&qualified).ok().flatten());
                let has_endpoint_rules = gateway_config
                    .map(|cfg| !cfg.endpoints.is_empty())
                    .unwrap_or(false);

                let url = if has_endpoint_rules {
                    format!(
                        "http://{}.{}.internal:{}",
                        bare_app_name,
                        qualified_app_id.namespace(),
                        self.internal_gateway_port
                    )
                } else {
                    format!("http://127.0.0.1:{}", addr.port())
                };

                tracing::info!(
                    key = %key,
                    url = %url,
                    app_id = %app_id.0,
                    has_endpoint_rules,
                    "[SPAWN] Injecting service discovery env var"
                );
                env_vars.retain(|(k, _)| k != &key);
                env_vars.push((key, url));
            }
        }

        let allowed_ports = {
            let mut ports = std::collections::HashSet::new();
            ports.insert(self.internal_gateway_port);
            for addrs in ns_services.values() {
                for addr in addrs {
                    ports.insert(addr.port());
                }
            }
            ports.insert(host_port);
            ports
        };

        let registry = self.service_registry.clone();
        let source_app = qualified_app_id.clone();
        let internal_gateway_port = self.internal_gateway_port;
        let instance_bind_ip = addr.ip();
        let socket_addr_check: SocketAddrCheckFn =
            Arc::new(move |dest: std::net::SocketAddr, use_type: SocketAddrUse| {
                let allowed = allowed_ports.clone();
                let registry = registry.clone();
                let source_app = source_app.clone();
                let instance_bind_ip = instance_bind_ip;
                Box::pin(async move {
                    tracing::info!(
                        source_app = %source_app.0,
                        dest = %dest,
                        use_type = ?use_type,
                        "[SOCKET DEBUG] socket_addr_check called"
                    );
                    match use_type {
                        SocketAddrUse::TcpConnect | SocketAddrUse::UdpSend => {
                            if !dest.ip().is_loopback() {
                                tracing::info!(
                                    dest = %dest,
                                    "[SOCKET DEBUG] external connection - allowed"
                                );
                                return true;
                            }

                            if !allowed.contains(&dest.port()) {
                                tracing::warn!(
                                    dest = %dest,
                                    "[SOCKET DEBUG] BLOCKED: unknown loopback port"
                                );
                                return false;
                            }

                            if dest.port() != internal_gateway_port {
                                let interceptor =
                                    NetworkInterceptor::new(registry, source_app.clone());
                                match interceptor
                                    .check_connect(
                                        std::net::SocketAddr::new(
                                            std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                                                127, 0, 0, 1,
                                            )),
                                            0,
                                        ),
                                        dest,
                                    )
                                    .await
                                {
                                    ConnectDecision::Allow(_) => {
                                        tracing::info!(
                                            dest = %dest,
                                            "[SOCKET DEBUG] same-namespace connection - allowed"
                                        );
                                        true
                                    }
                                    ConnectDecision::Deny { reason } => {
                                        tracing::warn!(
                                            dest = %dest,
                                            reason,
                                            "[SOCKET DEBUG] BLOCKED: cross-namespace"
                                        );
                                        false
                                    }
                                }
                            } else {
                                tracing::info!(
                                    dest = %dest,
                                    "[SOCKET DEBUG] internal gateway port - allowed"
                                );
                                true
                            }
                        }
                        SocketAddrUse::TcpBind
                        | SocketAddrUse::TcpListen
                        | SocketAddrUse::UdpBind => {
                            let ok = is_instance_bind_allowed(dest, &allowed, instance_bind_ip);
                            tracing::info!(
                                dest = %dest,
                                expected_bind_ip = %instance_bind_ip,
                                allowed = ok,
                                "[SOCKET DEBUG] bind check"
                            );
                            ok
                        }
                        _ => {
                            tracing::info!(
                                use_type = ?use_type,
                                "[SOCKET DEBUG] other socket use - allowed"
                            );
                            true
                        }
                    }
                })
            });

        // INTERNAL_APP_ID is intentionally not injected. The app should not
        // need awareness of its namespace to participate in service isolation.
        let app_id_clone = app_id.clone();
        let instance_id = InstanceId(uuid::Uuid::new_v4());

        let (task, shutdown_tx, instance_tid, instance_policy_counters) = match prepared
            .execution_model()
        {
            ComponentExecutionModel::WasiCli => {
                let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let prepared_clone = prepared.clone();
                let (spawn_result_tx, spawn_result_rx) =
                    tokio::sync::oneshot::channel::<Result<(), PlatformError>>();
                let namespace_map_for_spawn = self.namespace_map();
                let (tid_tx, tid_rx) = tokio::sync::oneshot::channel::<u32>();
                let (policy_counters_tx, policy_counters_rx) = tokio::sync::oneshot::channel();
                let (registration_tx, registration_rx) = std::sync::mpsc::sync_channel::<()>(0);
                let (execution_tx, execution_rx) = tokio::sync::oneshot::channel();

                let instance_thread = match std::thread::Builder::new()
                    .name("wasi-cli-instance".to_string())
                    .spawn(move || {
                        let tid = crate::gettid();
                        let _ = tid_tx.send(tid);
                        let _ = registration_rx.recv();

                        let mut instance = match prepared_clone.spawn_instance(
                            env_vars,
                            host_port,
                            Some(socket_addr_check),
                        ) {
                            Ok(instance) => instance,
                            Err(e) => {
                                let _ = spawn_result_tx.send(Err(PlatformError::runtime(format!(
                                    "Failed to spawn instance: {}",
                                    e
                                ))));
                                let _ = execution_tx.send(ExecutionStats {
                                    instance_id: InstanceId(uuid::Uuid::nil()),
                                    fuel_limit: 0,
                                    fuel_consumed: 0,
                                    ram_bytes: 0,
                                    wall_clock_ms: 0,
                                    trap: Some("spawn_failed".to_string()),
                                    io_stats: IoStats {
                                        open_fds_peak: 0,
                                        fs_bytes_written: 0,
                                        net_egress_bytes: 0,
                                        outbound_connections: 0,
                                    },
                                });
                                return;
                            }
                        };

                        let _ = policy_counters_tx.send(instance.policy_counters());
                        let _ = spawn_result_tx.send(Ok(()));
                        let stats = instance.run();

                        if let Some(ref trap) = stats.trap {
                            tracing::error!(
                                app = %app_id_clone.0,
                                fuel_consumed = stats.fuel_consumed,
                                ram_bytes = stats.ram_bytes,
                                trap = %trap,
                                "instance crashed with trap"
                            );
                        } else {
                            tracing::info!(
                                app = %app_id_clone.0,
                                fuel_consumed = stats.fuel_consumed,
                                ram_bytes = stats.ram_bytes,
                                "instance exited"
                            );
                        }
                        let _ = execution_tx.send(stats);
                    }) {
                    Ok(thread) => thread,
                    Err(e) => {
                        self.port_alloc.release(host_port);
                        return Err(PlatformError::runtime(format!(
                            "Failed to create dedicated wasi:cli thread: {e}"
                        )));
                    }
                };

                let instance_tid = tid_rx.await.map_err(|_| {
                    PlatformError::runtime("Dedicated wasi:cli thread did not report its TID")
                })?;
                if let Some(ref ns_map) = namespace_map_for_spawn {
                    let identity = ebpf_monitor::common::TidIdentity::new(
                        qualified_app_id.namespace(),
                        &qualified_app_id.0,
                    );
                    if let Err(e) = ns_map.register_tid(instance_tid, identity) {
                        tracing::warn!(
                            tid = instance_tid,
                            error = %e,
                            "Failed to register dedicated wasi:cli TID"
                        );
                    }
                }
                let _ = registration_tx.send(());

                match spawn_result_rx.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        self.port_alloc.release(host_port);
                        return Err(e);
                    }
                    Err(_) => {
                        self.port_alloc.release(host_port);
                        return Err(PlatformError::runtime(
                            "Spawn result channel closed unexpectedly",
                        ));
                    }
                }

                let namespace_map_for_cleanup = namespace_map_for_spawn.clone();
                let task = tokio::spawn(async move {
                    let stats = execution_rx.await.unwrap_or_else(|_| ExecutionStats {
                        instance_id: InstanceId(uuid::Uuid::nil()),
                        fuel_limit: 0,
                        fuel_consumed: 0,
                        ram_bytes: 0,
                        wall_clock_ms: 0,
                        trap: Some("dedicated wasi:cli thread terminated unexpectedly".to_string()),
                        io_stats: IoStats {
                            open_fds_peak: 0,
                            fs_bytes_written: 0,
                            net_egress_bytes: 0,
                            outbound_connections: 0,
                        },
                    });
                    if let Err(error) =
                        tokio::task::spawn_blocking(move || instance_thread.join()).await
                    {
                        tracing::warn!(%error, tid = instance_tid, "Failed to join dedicated wasi:cli thread");
                    }
                    if let Some(ref ns_map) = namespace_map_for_cleanup {
                        let _ = ns_map.deregister_tid(instance_tid);
                    }
                    stats
                });

                (
                    task,
                    shutdown_tx,
                    Some(instance_tid),
                    policy_counters_rx.await.ok(),
                )
            }
            ComponentExecutionModel::WasiHttpIncomingHandler => {
                let namespace_map_for_registration = self.namespace_map();
                let (tid_tx, tid_rx) = tokio::sync::oneshot::channel::<u32>();
                let (registration_tx, registration_rx) = std::sync::mpsc::sync_channel::<()>(0);
                let thread_start_hook = Box::new(move || {
                    let tid = crate::gettid();
                    let _ = tid_tx.send(tid);
                    let _ = registration_rx.recv();
                });

                let http_server = match prepared.spawn_http_server(
                    env_vars,
                    addr,
                    Some(socket_addr_check),
                    Some(thread_start_hook),
                ) {
                    Ok(server) => server,
                    Err(e) => {
                        self.port_alloc.release(host_port);
                        return Err(e);
                    }
                };

                let instance_tid = tid_rx.await.ok();
                if let Some(tid) = instance_tid {
                    if let Some(ref ns_map) = namespace_map_for_registration {
                        let identity = ebpf_monitor::common::TidIdentity::new(
                            qualified_app_id.namespace(),
                            &qualified_app_id.0,
                        );
                        if let Err(e) = ns_map.register_tid(tid, identity) {
                            tracing::warn!(
                                tid,
                                error = %e,
                                "Failed to register dedicated wasi:http TID"
                            );
                        }
                    }
                }
                let _ = registration_tx.send(());

                (
                    http_server.task,
                    http_server.shutdown_tx,
                    instance_tid,
                    Some(http_server.policy_counters),
                )
            }
        };

        if let Err(e) = crate::instance::wait_for_ready(addr, Duration::from_millis(500)).await {
            self.port_alloc.release(host_port);
            return Err(e);
        }

        self.upstream_registry
            .add(
                app_id,
                proxy::upstream::UpstreamEndpoint {
                    addr,
                    h2c: matches!(
                        prepared.execution_model(),
                        ComponentExecutionModel::WasiHttpIncomingHandler
                    ),
                },
            )
            .await;

        self.service_registry
            .register_endpoint(
                &qualified_app_id,
                crate::network::RegisteredEndpoint {
                    addr,
                    h2c: matches!(
                        prepared.execution_model(),
                        ComponentExecutionModel::WasiHttpIncomingHandler
                    ),
                },
            )
            .await;

        self.service_registry
            .bind_source_port(host_port, qualified_app_id.clone())
            .await;

        let tenant_id = config
            .tenant_id
            .clone()
            .unwrap_or_else(|| app_id.0.split(':').next().unwrap_or(&app_id.0).to_string());
        let fuel_quota = config.fuel_quota.0;
        let ram_bytes = config.memory_limit.to_bytes();

        let managed = ManagedInstance {
            id: instance_id.clone(),
            app_id: app_id.clone(),
            addr,
            state: InstanceState::Ready { addr },
            spawned_at: Instant::now(),
            last_request_at: Instant::now(),
            request_count: 0,
            task: Some(task),
            shutdown_tx: Some(shutdown_tx),
            billing_info: BillingInfo {
                tenant_id: tenant_id.clone(),
                fuel_quota,
                ram_bytes,
            },
            tid: instance_tid,
            policy_counters: instance_policy_counters,
            last_policy_export: crate::instance::PolicyCounterSnapshot::default(),
        };

        {
            let mut pools = self.pools.write().await;
            let pool = pools
                .entry(app_id.0.clone())
                .or_insert_with(|| InstancePool {
                    config,
                    prepared,
                    instances: Vec::new(),
                });
            pool.instances.push(managed);
        }

        let _ = self
            .event_tx
            .send(Event::InstanceReady {
                app_id: app_id.clone(),
                addr,
                node_id: self.node_id().to_string(),
            })
            .await;

        info!(app = %app_id.0, %addr, "instance ready");
        Ok(addr)
    }
}
