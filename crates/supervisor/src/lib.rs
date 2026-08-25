//! Supervisor orchestration surface for app instance lifecycle, with the
//! larger runtime flows split into focused sibling modules.

pub mod audit;
mod command_runtime;
pub mod config_validator;
pub mod db_proxy;
pub mod deployment;
mod health_provider;
mod health_runtime;

pub mod instance;
pub mod instance_manager;
pub mod network;
pub mod network_interceptor;
mod policy_metrics_runtime;
pub mod pool;
pub mod port_alloc;
pub mod scaling;
mod shutdown_runtime;
mod spawn_runtime;

#[cfg(test)]
mod tests;

use crate::{network::LocalServiceRegistry, pool::InstancePool, port_alloc::PortAllocator};
use common::{
    error::PlatformError,
    types::{AppConfig, AppId, InstanceId},
};
use messaging::events::Event;
use proxy::{router::HostRouter, upstream::UpstreamRegistry};
use runtime::WasmRuntime;
use std::{collections::HashMap, sync::Arc, time::Duration};
use storage::Store;
use tokio::sync::{mpsc, RwLock};
use tracing::warn;

type EnvResolver = dyn Fn(&AppConfig, u16) -> Vec<(String, String)> + Send + Sync;

pub(crate) fn is_instance_bind_allowed(
    dest: std::net::SocketAddr,
    allowed_ports: &std::collections::HashSet<u16>,
    instance_bind_ip: std::net::IpAddr,
) -> bool {
    allowed_ports.contains(&dest.port()) && dest.ip() == instance_bind_ip
}

/// Get the OS Thread ID (TID) of the current thread.
/// This is used for eBPF namespace enforcement registration.
#[cfg(target_os = "linux")]
fn gettid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(target_os = "linux"))]
fn gettid() -> u32 {
    0
}

// ---------------------------------------------------------------------
// Supervisor Command Interface
// ---------------------------------------------------------------------
/// Commands that request immediate operational actions from the supervisor.
///
/// These commands are processed asynchronously by the supervisor's command
/// loop, which runs in a background Tokio task started via
/// [`Supervisor::start_command_loop`].
#[derive(Debug)]
pub enum SupervisorCommand {
    /// Kill the instance consuming the most memory.
    /// Used when eBPF detects critical memory pressure or OOM.
    KillLargestInstance { reason: String },

    /// Kill all idle instances (those with no recent requests).
    /// Used when eBPF detects FD exhaustion approaching hard limit.
    PruneIdleInstances {
        /// Only prune instances idle for more than this many seconds.
        idle_threshold_secs: u64,
    },

    /// Remove an app's instances from the upstream routing table.
    /// Used when eBPF detects a process exit (the instance is dead).
    RemoveAppFromUpstream { app_id: AppId },

    /// Kill a specific instance by its app_id and instance_id.
    KillInstance {
        app_id: AppId,
        instance_id: InstanceId,
        reason: String,
    },

    /// Kill the application instance assigned to an eBPF-monitored OS thread.
    KillInstanceByTid { tid: u32, reason: String },
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub struct Supervisor {
    store: Store,
    /// Cached node ID (read from NODE_ID env var at construction time).
    node_id: String,
    runtime: WasmRuntime,
    port_alloc: Arc<PortAllocator>,
    upstream_registry: Arc<UpstreamRegistry>,
    pub(crate) host_router: Arc<HostRouter>,
    service_registry: Arc<LocalServiceRegistry>,
    internal_gateway_port: u16,
    env_resolver: Arc<EnvResolver>,

    /// Map of `app_id` to instance pool.
    pools: Arc<RwLock<HashMap<String, InstancePool>>>,

    /// Channel to publish events to NATS
    event_tx: mpsc::Sender<Event>,

    /// Channel to send log records
    log_tx: Option<mpsc::Sender<metrics::WasmLogRecord>>,

    /// Prometheus policy metrics sink. Set after Metrics initialization in node startup.
    policy_metrics: std::sync::RwLock<Option<Arc<metrics::exporter::PolicyMetrics>>>,

    /// Channel to send billing records
    billing_tx: Option<mpsc::Sender<billing::BillingInput>>,

    /// Channel to receive commands from eBPF monitor and other subsystems.
    /// Wrapped in `Mutex` so the command loop can take it out via `Option::take`
    /// even though `Supervisor` is behind `Arc`.
    command_rx: std::sync::Mutex<Option<mpsc::Receiver<SupervisorCommand>>>,

    /// Sender clone for providing to eBPF monitor callbacks and admin API.
    command_tx: mpsc::Sender<SupervisorCommand>,

    /// Watch receiver for the health-check interval (hot-reloadable).
    /// When the operator changes `health.check_interval_secs` via the admin
    /// API, the sender side is updated and this receiver picks up the new
    /// value on the next loop iteration.
    health_interval_rx: Option<tokio::sync::watch::Receiver<u64>>,

    /// eBPF namespace map for TID registration and identity resolution.
    /// Shared with the internal gateway.
    namespace_map: std::sync::RwLock<Option<Arc<ebpf_monitor::NamespaceMap>>>,
}

impl Supervisor {
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn new(
        store: Store,
        node_id: String,
        runtime: WasmRuntime,
        port_alloc: Arc<PortAllocator>,
        upstream_registry: Arc<UpstreamRegistry>,
        host_router: Arc<HostRouter>,
        service_registry: Arc<LocalServiceRegistry>,
        internal_gateway_port: u16,
        env_resolver: Arc<EnvResolver>,
        event_tx: mpsc::Sender<Event>,
        billing_tx: Option<mpsc::Sender<billing::BillingInput>>,
    ) -> Arc<Self> {
        let (command_tx, command_rx) = mpsc::channel::<SupervisorCommand>(256);
        Arc::new(Self {
            store,
            node_id,
            runtime,
            port_alloc,
            upstream_registry,
            host_router,
            service_registry,
            internal_gateway_port,
            env_resolver,
            pools: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            log_tx: None,
            policy_metrics: std::sync::RwLock::new(None),
            billing_tx,
            command_rx: std::sync::Mutex::new(Some(command_rx)),
            command_tx,
            health_interval_rx: None,
            namespace_map: std::sync::RwLock::new(None),
        })
    }

    /// Set the eBPF namespace map for TID registration and identity resolution.
    /// Called by the node after initializing the eBPF monitor.
    pub fn set_namespace_map(&self, namespace_map: Arc<ebpf_monitor::NamespaceMap>) {
        if let Ok(mut slot) = self.namespace_map.write() {
            *slot = Some(namespace_map);
        }
    }

    pub(crate) fn namespace_map(&self) -> Option<Arc<ebpf_monitor::NamespaceMap>> {
        self.namespace_map
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().cloned())
    }

    /// Set the watch receiver for the health-check interval.
    ///
    /// Call this after construction, before `start_health_loop()`.
    /// The sender side is typically held by the config sync task in
    /// `main.rs` which reads from `HotConfigHandle`.
    pub fn set_health_interval_rx(&mut self, rx: tokio::sync::watch::Receiver<u64>) {
        self.health_interval_rx = Some(rx);
    }

    pub fn set_log_dispatcher(&mut self, log_tx: mpsc::Sender<metrics::WasmLogRecord>) {
        self.log_tx = Some(log_tx);
    }

    pub fn set_policy_metrics(&self, policy_metrics: Arc<metrics::exporter::PolicyMetrics>) {
        if let Ok(mut slot) = self.policy_metrics.write() {
            *slot = Some(policy_metrics);
        }
    }

    /// Get a sender for the supervisor command channel.
    ///
    /// This can be used by the eBPF monitor callbacks, admin API handlers,
    /// or any other subsystem that needs to request immediate supervisor
    /// actions (kill largest instance, prune idle, etc.).
    pub fn command_tx(&self) -> mpsc::Sender<SupervisorCommand> {
        self.command_tx.clone()
    }

    fn send_billing_record(&self, input: billing::BillingInput) {
        if let Some(ref tx) = self.billing_tx {
            if let Err(e) = tx.try_send(input) {
                warn!(error = %e, "billing channel full - dropping record");
            }
        }
    }

    fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn upstream(&self) -> &Arc<UpstreamRegistry> {
        &self.upstream_registry
    }

    /// Get a reference to the host router.
    /// Public accessor for the `pub(crate)` field.
    pub fn host_router(&self) -> &Arc<HostRouter> {
        &self.host_router
    }

    pub async fn list_instances(&self, app_id: &AppId) -> Vec<InstanceId> {
        let pools = self.pools.read().await;
        pools
            .get(&app_id.0)
            .map(|pool| pool.instances.iter().map(|i| i.id.clone()).collect())
            .unwrap_or_default()
    }

    pub async fn node_stats(&self) -> Result<scaling::NodeStats, PlatformError> {
        let mut total_instances = 0;
        let mut app_counts = HashMap::new();

        let pools = self.pools.read().await;
        for (app_id, pool) in pools.iter() {
            let count = pool.active_count();
            if count > 0 {
                app_counts.insert(app_id.clone(), count);
                total_instances += count;
            }
        }

        Ok(scaling::NodeStats {
            cpu_percent: 0.0, // placeholder
            fuel_per_sec: 0,  // placeholder
            total_instances,
            app_counts,
        })
    }

    /// List all app IDs currently managed by the supervisor.
    pub async fn list_app_ids(&self) -> Vec<AppId> {
        let pools = self.pools.read().await;
        pools.keys().map(|k| AppId(k.clone())).collect()
    }

    /// Get all registered service addresses for service discovery.
    pub fn get_service_registry(&self) -> Arc<LocalServiceRegistry> {
        self.service_registry.clone()
    }

    // -----------------------------------------------------------------
    // Command Loop (for eBPF monitor / admin API integration)
    // -----------------------------------------------------------------
    ///
    /// Listens for [`SupervisorCommand`] messages on the command channel
    /// and dispatches them to the appropriate supervisor methods. This
    /// enables the eBPF monitor and admin API to request immediate actions
    /// (kill instances, prune idle, etc.) without needing direct `Arc<Supervisor>`
    /// access or async runtime handles.
    ///
    /// Should be called once during startup, after `start_health_loop`.
    pub fn start_command_loop(self: Arc<Self>) {
        command_runtime::start_command_loop(self);
    }

    /// Kill the instance consuming the most memory.
    ///
    /// Scans all instance pools and finds the ready instance with the
    /// highest `ram_bytes` in its billing info. Used by the eBPF monitor
    /// when critical memory pressure or OOM is detected.
    pub async fn kill_largest_instance(&self, reason: &str) {
        command_runtime::kill_largest_instance(self, reason).await;
    }

    /// Kill all idle instances across all apps.
    ///
    /// An instance is considered idle if it hasn't received a request in
    /// more than `idle_threshold_secs` seconds. Used by the eBPF monitor
    /// when FD exhaustion is approaching the hard limit.
    pub async fn prune_idle_instances(&self, idle_threshold_secs: u64) {
        command_runtime::prune_idle_instances(self, idle_threshold_secs).await;
    }

    /// Gracefully shutdown all instances across all apps.
    /// Used during node shutdown (SIGTERM).
    pub async fn shutdown_all(&self, timeout: Duration) {
        command_runtime::shutdown_all(self, timeout).await;
    }
}
