//! Cluster event handling entrypoint.
//!
//! The event dispatcher stays here while heavier domain logic lives in focused
//! submodules for deploy ingestion, bootstrap, cluster runtime updates, and
//! upgrade/drain behavior.

use common::{
    artifact_transfer::{
        ArtifactManifestAudienceBinding, ArtifactTransferAuthority,
        BootstrapArtifactFetchAuthorization, SignedArtifactTransferManifest,
        ARTIFACT_TRANSFER_MANIFEST_HEADER, ARTIFACT_TRANSFER_REQUESTER_NODE_HEADER,
    },
    error::PlatformError,
    types::{AppId, ClusterNodeRecord},
};
use messaging::events::Event;
use proxy::dns_webhook::DnsWebhookManager;
use proxy::node_table::NodeLoadTable;
use proxy::router::HostRouter;
use proxy::upstream::UpstreamRegistry;
use reqwest::Url;
use runtime::WasmRuntime;
use secrets::{BootstrapKeyPair, SecretProvider, SecretTransportEntry};
use sha2::Digest;
use std::sync::Arc;
use storage::Store;
use supervisor::Supervisor;
use tokio::sync::Mutex;
use tracing::{info, warn};

mod bootstrap;
mod cluster_runtime;
mod deploy_intent;
mod upgrade_runtime;

use bootstrap::BootstrapContext;
pub(crate) use bootstrap::{apply_secret_update, BootstrapSessionState};
use cluster_runtime::ClusterRuntimeContext;
#[allow(dead_code)]
pub(crate) const BOOTSTRAP_APPLIED_META_KEY: &str = bootstrap::BOOTSTRAP_APPLIED_META_KEY;
#[allow(dead_code)]
pub(crate) const BOOTSTRAP_PENDING_META_KEY: &str = bootstrap::BOOTSTRAP_PENDING_META_KEY;
#[cfg(test)]
pub(crate) use deploy_intent::artifact_credentials_app_id;
pub use deploy_intent::{
    ingest_remote_artifact, oci_reference_is_digest_pinned, process_deploy_intent,
    DeployIntentContext,
};
#[cfg(test)]
use deploy_intent::{
    is_loopback_artifact_url, normalized_host_architecture, validate_peer_artifact_url,
    MAX_REMOTE_ARTIFACT_BYTES,
};
use upgrade_runtime::UpgradeContext;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn merge_cluster_node_record(
    existing: Option<ClusterNodeRecord>,
    node_id: &str,
) -> ClusterNodeRecord {
    existing.unwrap_or_else(|| ClusterNodeRecord::new(node_id.to_string(), now_unix_secs()))
}

fn extract_proxy_host(address: &str) -> Option<String> {
    Url::parse(&format!("http://{address}"))
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
}

pub struct EventDispatcher {
    pub supervisor: Arc<Supervisor>,
    pub upstream: Arc<UpstreamRegistry>,
    pub host_router: Arc<HostRouter>,
    pub store: Store,
    pub runtime: WasmRuntime,
    pub node_id: String,
    pub artifact_server_url: String,
    pub artifact_transfer_authority: ArtifactTransferAuthority,
    pub upgrade_signing_public_key: Option<String>,
    pub secret_provider: Arc<dyn SecretProvider>,
    pub secret_transport_keypair: Arc<BootstrapKeyPair>,
    pub(crate) bootstrap_session: Option<Arc<Mutex<BootstrapSessionState>>>,
    pub bus: messaging::NatsBus,
    pub dns_webhook: Option<DnsWebhookManager>,
    pub node_table: Arc<NodeLoadTable>,
    pub cluster_node_stale_after_secs: u64,
    /// In-memory gateway cache (also updated when persistent storage changes).
    pub gateway: Option<Arc<proxy::gateway::Gateway>>,
}

impl EventDispatcher {
    pub async fn handle(&self, event: Event) -> Result<(), PlatformError> {
        let event_name = format!("{:?}", std::mem::discriminant(&event));
        tracing::info!(event = %event_name, "received event in handler");

        match event {
            Event::DeployApp {
                app_id,
                config,
                artifact_url,
                artifact_transfer_manifests,
                expected_hash,
                size_bytes,
            } => {
                info!(
                    "🚀 Handling DeployApp for app_id: {}, url: {}",
                    app_id.0, artifact_url
                );
                self.handle_deploy(
                    app_id,
                    config,
                    artifact_url,
                    artifact_transfer_manifests,
                    expected_hash,
                    size_bytes,
                )
                .await
            }
            Event::RemoveApp { app_id } => self.handle_remove(app_id).await,
            Event::RouteAdd { route } => self.handle_route_add(route).await,
            Event::RouteRemove { host } => self.handle_route_remove(host).await,
            Event::InstanceReady {
                app_id,
                addr,
                node_id,
            } => {
                self.handle_remote_instance_ready(app_id, addr, node_id)
                    .await
            }
            Event::InstanceDead {
                app_id,
                addr,
                node_id,
            } => {
                self.handle_remote_instance_dead(app_id, addr, node_id)
                    .await
            }
            Event::SecretUpdate {
                app_id,
                key,
                target_node_id,
                secret,
            } => {
                if let Some(target_node_id) = target_node_id {
                    if target_node_id != self.our_node_id() {
                        return Ok(());
                    }
                }
                info!(app = %app_id.0, key, "received secret rotation");
                apply_secret_update(
                    self.secret_provider.as_ref(),
                    self.secret_transport_keypair.as_ref(),
                    &app_id,
                    &key,
                    &secret,
                )
                .await?;
                Ok(())
            }
            Event::ConfigUpdate { app_id, config } => {
                self.store.save_config(&config)?;
                info!(app = %app_id.0, "config updated");
                Ok(())
            }
            Event::NodeLoad {
                node_id,
                cpu_percent: _,
                fuel_budget_used_percent,
                active_instances,
                proxy_address,
            } => {
                self.handle_node_load(
                    node_id,
                    fuel_budget_used_percent,
                    active_instances,
                    proxy_address,
                )
                .await
            }
            Event::NodeJoined {
                node_id,
                bootstrap_session_id,
                bootstrap_nonce,
                artifact_server_url,
                public_key_bytes,
                protocol_version,
                binary_version,
            } => {
                self.handle_node_joined(
                    node_id,
                    bootstrap_session_id,
                    bootstrap_nonce,
                    artifact_server_url,
                    public_key_bytes,
                    protocol_version,
                    binary_version,
                )
                .await
            }
            Event::StateSnapshot {
                for_node_id,
                bootstrap_session_id,
                bootstrap_nonce,
                configs,
                routes,
                encrypted_secrets,
                gateway_configs,
                api_keys,
                artifact_fetches,
                artifact_hashes,
            } => {
                if for_node_id == self.node_id {
                    self.handle_state_snapshot(
                        bootstrap_session_id,
                        bootstrap_nonce,
                        configs,
                        routes,
                        encrypted_secrets,
                        gateway_configs,
                        api_keys,
                        artifact_fetches,
                        artifact_hashes,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            Event::NodeUpgrade { .. } => {
                self.handle_node_upgrade(event).await;
                Ok(())
            }
            Event::NodeUpgradeComplete {
                node_id,
                new_binary_version,
                new_protocol_version,
            } => {
                info!(
                    node = %node_id,
                    version = %new_binary_version,
                    protocol = new_protocol_version,
                    "node upgrade completed"
                );
                // Check if we were waiting for this node
                // Re-evaluate our upgrade status if we're in a rolling upgrade
                Ok(())
            }
            Event::NodeDraining {
                node_id,
                drain_timeout_secs,
            } => {
                if node_id == self.node_id {
                    info!(
                        timeout_secs = drain_timeout_secs,
                        "beginning graceful shutdown"
                    );
                    self.begin_graceful_shutdown(drain_timeout_secs).await;
                } else {
                    info!(node = %node_id, "peer node draining");
                }
                Ok(())
            }

            // ── eBPF Monitor Events ──────────────────────────────────────
            Event::NodeUnderPressure {
                node_id,
                pressure_level,
            } => {
                self.handle_node_under_pressure(node_id, pressure_level)
                    .await
            }

            Event::NodePressureRecovered { node_id } => {
                self.handle_node_pressure_recovered(node_id).await
            }

            Event::SecurityIncident {
                node_id,
                app_id,
                pid,
                syscall_nr,
                category,
            } => {
                tracing::error!(
                    node = %node_id,
                    app = %app_id,
                    pid,
                    syscall_nr,
                    category,
                    "SECURITY INCIDENT: privileged syscall detected by eBPF monitor"
                );
                // If the incident is from our node, the eBPF ActionDispatcher
                // already killed the instance. For remote nodes, we log and
                // could quarantine the artifact hash if we had a cluster-wide
                // artifact blocklist. For now, we just log the incident.
                if node_id != self.node_id {
                    warn!(
                        node = %node_id,
                        app = %app_id,
                        "security incident on peer node — consider quarantining artifact"
                    );
                }
                Ok(())
            }

            // ── Health Events ─────────────────────────────────────────────
            Event::NodeHealthChanged {
                node_id,
                status,
                cause,
                active_instances,
                accepting_requests,
                timestamp: _,
            } => {
                tracing::info!(
                    node = %node_id,
                    status = %status,
                    cause = ?cause,
                    active_instances,
                    accepting_requests,
                    "node health status changed"
                );
                self.handle_node_health_changed(
                    node_id,
                    status,
                    active_instances,
                    accepting_requests,
                )
                .await
            }

            Event::NodeHealthSnapshot {
                node_id,
                status,
                active_instances,
                deployed_apps,
                nats_connected,
                disk_free_mb,
                memory_used_mb,
                ..
            } => {
                tracing::debug!(
                    node = %node_id,
                    status = %status,
                    active_instances,
                    deployed_apps,
                    nats_connected,
                    disk_free_mb,
                    memory_used_mb,
                    "node health snapshot received"
                );
                self.handle_node_health_snapshot(node_id, status, active_instances, deployed_apps)
                    .await
            }

            Event::GatewayConfigUpdate { app_id, config } => {
                self.handle_gateway_config_update(app_id, config).await
            }
            Event::GatewayConfigRemove { app_id } => {
                self.handle_gateway_config_remove(app_id).await
            }
            // ── Configuration Hot-Reload ──────────────────────────────────
            Event::ConfigHotReload { node_id, changes } => {
                tracing::info!(
                    node = %node_id,
                    changes = ?changes,
                    "peer node changed hot-reloadable config (informational only, not auto-applied)"
                );
                // Design decision: Config changes are NOT auto-propagated.
                // Each node's operator controls its own configuration.
                // The event is informational — it alerts operators that the
                // cluster's configuration may have diverged.
                Ok(())
            }
            Event::DeployIngressArtifactReplicated { .. } => Ok(()),
        }
    }

    async fn handle_deploy(
        &self,
        app_id: AppId,
        config: common::types::AppConfig,
        artifact_url: String,
        artifact_transfer_manifests: Vec<ArtifactManifestAudienceBinding>,
        expected_hash: Option<String>,
        size_bytes: u64,
    ) -> Result<(), PlatformError> {
        tracing::info!(app = %app_id.0, "handle_deploy invoked");

        let sha256 = expected_hash
            .clone()
            .ok_or_else(|| PlatformError::messaging("deploy event missing expected_hash"))?;

        info!(
            app = %app_id.0,
            url = %artifact_url,
            size_mb = size_bytes as f64 / 1_048_576.0,
            "deploying artifact"
        );

        // 1. Check local cache first (another node may have already stored it)
        let wasm_bytes = if self.store.raw_wasm_exists(&sha256)? {
            info!(sha256, "artifact already in local cache");
            self.store.load_raw_wasm(&sha256)?.ok_or_else(|| {
                PlatformError::storage("artifact vanished between exists and load")
            })?
        } else {
            // 2. Fetch from the source node
            info!(url = %artifact_url, "fetching artifact via HTTP");
            let targeted_manifest = artifact_transfer_manifests
                .iter()
                .find(|binding| binding.audience_node_id == self.node_id)
                .map(|binding| &binding.artifact_transfer_manifest)
                .ok_or_else(|| {
                    PlatformError::messaging(format!(
                        "deploy event missing audience-bound artifact manifest for node {} (artifact {})",
                        self.node_id, sha256
                    ))
                })?;
            let bytes = fetch_artifact(
                &artifact_url,
                Some(self.node_id.as_str()),
                Some(targeted_manifest),
                &sha256,
            )
            .await
            .map_err(PlatformError::external)?;

            // Hash already verified by download_and_verify_bytes.
            // Persisting raw bytes is part of successful deploy processing.
            self.store.save_raw_wasm(&sha256, &bytes)?;
            bytes
        };

        info!(app = %app_id.0, bytes = wasm_bytes.len(), "artifact ready, compiling");

        // 5. Compile (CPU-intensive — spawn_blocking)
        let runtime = self.runtime.clone();
        let artifact = tokio::task::spawn_blocking(move || runtime.compile(&wasm_bytes))
            .await
            .map_err(|e| PlatformError::runtime(format!("spawn_blocking panic: {e}")))?;
        let artifact_bytes = artifact?;

        // 6. Store compiled artifact, config, and hash
        self.store.store_artifact(&app_id, &artifact_bytes)?;
        self.store.save_config(&config)?;
        self.store.save_artifact_hash(&app_id, &sha256)?;
        info!(
            app = %app_id.0,
            "deploy complete, waiting for first request"
        );
        Ok(())
    }

    async fn handle_remove(&self, app_id: AppId) -> Result<(), PlatformError> {
        info!(app = %app_id.0, "removing app");

        // Kill all running instances first (this creates billing records)
        self.supervisor.kill_all_instances(&app_id).await?;

        // Mark app as undeployed - starts grace period
        // Actual deletion happens after grace period expires in GC loop
        self.store.mark_undeployed(&app_id.0)?;

        // Note: We don't immediately delete artifacts/configs anymore
        // The GC loop will purge them after the grace period
        info!(app = %app_id.0, "app marked for deletion, grace period started");
        Ok(())
    }

    fn our_node_id(&self) -> String {
        self.node_id.clone()
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_node_joined(
        &self,
        new_node_id: String,
        bootstrap_session_id: String,
        bootstrap_nonce: String,
        peer_artifact_url: String,
        peer_public_key: Vec<u8>,
        protocol_version: u32,
        binary_version: String,
    ) -> Result<(), PlatformError> {
        info!(
            new_node = %new_node_id,
            protocol = protocol_version,
            version = %binary_version,
            "node joined cluster"
        );

        if new_node_id == self.node_id {
            tracing::debug!(new_node = %new_node_id, "ignoring our own NodeJoined event");
            return Ok(());
        }
        bootstrap::handle_node_joined(
            BootstrapContext {
                supervisor: &self.supervisor,
                upstream: &self.upstream,
                host_router: &self.host_router,
                store: &self.store,
                runtime: &self.runtime,
                node_id: &self.node_id,
                artifact_server_url: &self.artifact_server_url,
                artifact_transfer_authority: &self.artifact_transfer_authority,
                secret_provider: &self.secret_provider,
                bootstrap_session: self.bootstrap_session.as_ref(),
                bus: &self.bus,
                gateway: self.gateway.as_ref(),
            },
            new_node_id,
            bootstrap_session_id,
            bootstrap_nonce,
            peer_artifact_url,
            peer_public_key,
            protocol_version,
            binary_version,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_state_snapshot(
        &self,
        bootstrap_session_id: String,
        bootstrap_nonce: String,
        configs: Vec<common::types::AppConfig>,
        routes: Vec<common::types::Route>,
        encrypted_secrets: Vec<SecretTransportEntry>,
        gateway_configs: Vec<(String, common::types::GatewayRouteConfig)>,
        api_keys: Vec<(String, Vec<common::types::ApiKeyRecord>)>,
        artifact_fetches: Vec<BootstrapArtifactFetchAuthorization>,
        artifact_hashes: Vec<(String, String)>,
    ) -> Result<(), PlatformError> {
        bootstrap::handle_state_snapshot(
            BootstrapContext {
                supervisor: &self.supervisor,
                upstream: &self.upstream,
                host_router: &self.host_router,
                store: &self.store,
                runtime: &self.runtime,
                node_id: &self.node_id,
                artifact_server_url: &self.artifact_server_url,
                artifact_transfer_authority: &self.artifact_transfer_authority,
                secret_provider: &self.secret_provider,
                bootstrap_session: self.bootstrap_session.as_ref(),
                bus: &self.bus,
                gateway: self.gateway.as_ref(),
            },
            bootstrap_session_id,
            bootstrap_nonce,
            configs,
            routes,
            encrypted_secrets,
            gateway_configs,
            api_keys,
            artifact_fetches,
            artifact_hashes,
        )
        .await
    }

    async fn handle_node_upgrade(&self, event: Event) {
        upgrade_runtime::handle_node_upgrade(
            UpgradeContext {
                supervisor: &self.supervisor,
                store: &self.store,
                node_id: &self.node_id,
                cluster_node_stale_after_secs: self.cluster_node_stale_after_secs,
                upgrade_signing_public_key: self.upgrade_signing_public_key.as_deref(),
                bus: &self.bus,
            },
            event,
        )
        .await
    }

    async fn begin_graceful_shutdown(&self, timeout_secs: u64) {
        upgrade_runtime::begin_graceful_shutdown(&self.supervisor, timeout_secs).await;
    }

    fn cluster_runtime_context(&self) -> ClusterRuntimeContext<'_> {
        ClusterRuntimeContext {
            host_router: &self.host_router,
            store: &self.store,
            dns_webhook: self.dns_webhook.as_ref(),
            node_table: &self.node_table,
            gateway: self.gateway.as_ref(),
        }
    }

    async fn handle_route_add(&self, route: common::types::Route) -> Result<(), PlatformError> {
        cluster_runtime::handle_route_add(self.cluster_runtime_context(), route).await
    }

    async fn handle_route_remove(&self, host: String) -> Result<(), PlatformError> {
        cluster_runtime::handle_route_remove(self.cluster_runtime_context(), host).await
    }

    async fn handle_node_load(
        &self,
        node_id: String,
        fuel_budget_used_percent: f32,
        active_instances: u32,
        proxy_address: String,
    ) -> Result<(), PlatformError> {
        cluster_runtime::handle_node_load(
            self.cluster_runtime_context(),
            node_id,
            fuel_budget_used_percent,
            active_instances,
            proxy_address,
        )
        .await
    }

    async fn handle_node_health_changed(
        &self,
        node_id: String,
        status: String,
        active_instances: u32,
        accepting_requests: bool,
    ) -> Result<(), PlatformError> {
        cluster_runtime::handle_node_health_changed(
            self.cluster_runtime_context(),
            node_id,
            status,
            active_instances,
            accepting_requests,
        )
        .await
    }

    async fn handle_node_health_snapshot(
        &self,
        node_id: String,
        status: String,
        active_instances: u32,
        deployed_apps: u32,
    ) -> Result<(), PlatformError> {
        cluster_runtime::handle_node_health_snapshot(
            self.cluster_runtime_context(),
            node_id,
            status,
            active_instances,
            deployed_apps,
        )
        .await
    }

    async fn handle_gateway_config_update(
        &self,
        app_id: AppId,
        config: common::types::GatewayRouteConfig,
    ) -> Result<(), PlatformError> {
        cluster_runtime::handle_gateway_config_update(
            self.cluster_runtime_context(),
            app_id,
            config,
        )
        .await
    }

    async fn handle_gateway_config_remove(&self, app_id: AppId) -> Result<(), PlatformError> {
        cluster_runtime::handle_gateway_config_remove(self.cluster_runtime_context(), app_id).await
    }

    /// Register a peer-owned instance with the local upstream table.
    ///
    /// Local instances are tracked directly by the supervisor, so this only
    /// handles cross-node announcements.
    async fn handle_remote_instance_ready(
        &self,
        app_id: AppId,
        addr: std::net::SocketAddr,
        node_id: String,
    ) -> Result<(), PlatformError> {
        if node_id != self.our_node_id() {
            self.upstream
                .add(
                    &app_id,
                    proxy::upstream::UpstreamEndpoint { addr, h2c: false },
                )
                .await;
            info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance registered");
        }
        Ok(())
    }

    async fn handle_remote_instance_dead(
        &self,
        app_id: AppId,
        addr: std::net::SocketAddr,
        node_id: String,
    ) -> Result<(), PlatformError> {
        if node_id != self.our_node_id() {
            self.upstream.remove(&app_id, &addr).await;
            info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance deregistered");
        }
        Ok(())
    }

    async fn handle_node_under_pressure(
        &self,
        node_id: String,
        pressure_level: u32,
    ) -> Result<(), PlatformError> {
        if node_id == self.node_id {
            warn!(
                pressure_level,
                "local node under pressure — excluding self from least-loaded routing"
            );
            self.node_table.mark_unhealthy(&node_id).await;
        } else {
            warn!(
                node = %node_id,
                pressure_level,
                "peer node under pressure — removing from routing"
            );
            self.node_table.mark_unhealthy(&node_id).await;
            if pressure_level >= 2 {
                tracing::warn!(
                    node = %node_id,
                    "peer node under CRITICAL pressure — removing all upstream entries"
                );
            }
        }
        Ok(())
    }

    async fn handle_node_pressure_recovered(&self, node_id: String) -> Result<(), PlatformError> {
        if node_id == self.node_id {
            info!("our node pressure recovered");
            self.node_table.mark_healthy(&node_id).await;
        } else {
            info!(node = %node_id, "peer node pressure recovered — restoring in routing");
            self.node_table.mark_healthy(&node_id).await;
        }
        Ok(())
    }
}

/// Fetch an artifact from a URL and verify its SHA-256 hash.
async fn fetch_artifact(
    url: &str,
    requester_node_id: Option<&str>,
    artifact_transfer_manifest: Option<&SignedArtifactTransferManifest>,
    expected_sha256: &str,
) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::new();
    let mut request = client.get(url);
    if let Some(manifest) = artifact_transfer_manifest {
        let header_value = manifest.encode_header_value()?;
        request = request.header(ARTIFACT_TRANSFER_MANIFEST_HEADER, header_value);
        if manifest.manifest.audience.is_some() {
            if let Some(requester_node_id) = requester_node_id {
                request =
                    request.header(ARTIFACT_TRANSFER_REQUESTER_NODE_HEADER, requester_node_id);
            }
        }
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("download failed with HTTP {}", response.status()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read artifact response body: {e}"))?;
    let actual_sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    if actual_sha256 != expected_sha256 {
        return Err(format!(
            "artifact hash mismatch: expected {}, got {}",
            expected_sha256, actual_sha256
        ));
    }

    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::{
        apply_secret_update, artifact_credentials_app_id, fetch_artifact, ingest_remote_artifact,
        is_loopback_artifact_url, normalized_host_architecture, validate_peer_artifact_url,
        BootstrapSessionState, EventDispatcher,
    };
    use common::{
        artifact_transfer::{
            ArtifactTransferAuthority, ARTIFACT_TRANSFER_MANIFEST_HEADER,
            ARTIFACT_TRANSFER_REQUESTER_NODE_HEADER,
        },
        types::AppId,
    };
    use e2e::NatsContainer;
    use messaging::{events::Event, NatsBus};
    use secrets::{
        crypto::SymmetricKey, encrypt_for_peer, BootstrapKeyPair, LocalSecretProvider,
        SecretProvider, SecretTransportEnvelope,
    };
    use sha2::Digest;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;
    use storage::Store;
    use supervisor::{network::NamespaceRegistry, port_alloc::PortAllocator, Supervisor};
    use tempfile::NamedTempFile;
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot, Mutex};

    static NATS_PORT_COUNTER: AtomicU16 = AtomicU16::new(0);

    fn allocate_nats_port() -> u16 {
        let base = 25000 + ((std::process::id() as u16) % 1000);
        base + NATS_PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    async fn start_test_nats() -> Result<NatsContainer, String> {
        NatsContainer::start(allocate_nats_port()).await
    }

    async fn build_test_dispatcher(
        store: Store,
        bootstrap_session: Option<Arc<Mutex<BootstrapSessionState>>>,
        dns_webhook: Option<proxy::dns_webhook::DnsWebhookManager>,
    ) -> EventDispatcher {
        let runtime = runtime::WasmRuntime::new().unwrap();
        let upstream = Arc::new(proxy::upstream::UpstreamRegistry::new());
        let host_router = Arc::new(proxy::router::HostRouter::default());
        let service_registry = Arc::new(NamespaceRegistry::default());
        let port_alloc = Arc::new(PortAllocator::new(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            20000,
            20010,
        ));
        let (event_tx, _event_rx) = mpsc::channel(8);
        let supervisor = Supervisor::new(
            store.clone(),
            "node-under-test".to_string(),
            runtime.clone(),
            port_alloc,
            upstream.clone(),
            host_router.clone(),
            service_registry,
            0,
            Arc::new(|_, _| Vec::new()),
            event_tx,
            None,
        );
        let nats = start_test_nats().await.unwrap();
        let mut bus = NatsBus::connect(&nats.url).await.unwrap();
        bus.set_node_id("node-under-test".to_string());

        EventDispatcher {
            supervisor,
            upstream,
            host_router,
            store: store.clone(),
            runtime,
            node_id: "node-under-test".to_string(),
            artifact_server_url: "http://node-under-test.internal:9091".to_string(),
            artifact_transfer_authority: ArtifactTransferAuthority::derive(
                "node-under-test",
                &[9u8; 32],
            ),
            upgrade_signing_public_key: None,
            secret_provider: Arc::new(LocalSecretProvider::new(store, SymmetricKey::generate())),
            secret_transport_keypair: Arc::new(BootstrapKeyPair::generate()),
            bootstrap_session,
            bus,
            dns_webhook,
            node_table: Arc::new(proxy::node_table::NodeLoadTable::default()),
            cluster_node_stale_after_secs: 120,
            gateway: None,
        }
    }

    #[test]
    fn test_is_loopback_artifact_url_detects_loopback_hosts() {
        assert!(is_loopback_artifact_url("http://127.0.0.1:9091"));
        assert!(is_loopback_artifact_url("http://localhost:9091"));
        assert!(!is_loopback_artifact_url("http://node-1.internal:9091"));
    }

    #[test]
    fn test_validate_peer_artifact_url_rejects_loopback() {
        let err = validate_peer_artifact_url("node-1", "http://127.0.0.1:9091").unwrap_err();
        assert!(err.to_string().contains("loopback artifact URL"));
    }

    #[test]
    fn test_validate_peer_artifact_url_accepts_routable_url() {
        validate_peer_artifact_url("node-1", "http://node-1.internal:9091").unwrap();
    }

    #[tokio::test]
    async fn test_apply_secret_update_uses_secret_provider_bundle_format() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        let app_id = AppId("secret-app:v1".to_string());

        apply_secret_update(
            &provider,
            &BootstrapKeyPair::generate(),
            &app_id,
            "API_KEY",
            &SecretTransportEnvelope::plaintext_utf8("super-secret-value"),
        )
        .await
        .unwrap();

        let plaintext = provider.get(&app_id, "API_KEY").await.unwrap();
        assert_eq!(plaintext, "super-secret-value");

        let raw = store.load_secrets(&app_id).unwrap().unwrap();
        assert_ne!(raw, b"super-secret-value");
    }

    #[tokio::test]
    async fn test_secret_update_event_roundtrip_persists_plaintext_via_secret_provider() {
        let _nats = start_test_nats().await.unwrap();
        let bus = NatsBus::connect(&_nats.url).await.unwrap();
        bus.setup_jetstream().await.unwrap();

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = std::sync::Arc::new(LocalSecretProvider::new(
            store.clone(),
            SymmetricKey::generate(),
        ));
        let app_id = AppId("secret-app:v1".to_string());
        let key = "API_KEY".to_string();
        let expected_value = "super-secret-over-nats".to_string();
        let (tx, rx) = oneshot::channel();
        let provider_for_handler = provider.clone();
        let app_id_for_handler = app_id.clone();
        let key_for_handler = key.clone();
        let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let tx_for_handler = tx.clone();

        bus.subscribe(&format!("secrets.update.{}", app_id.0), move |event| {
            let provider = provider_for_handler.clone();
            let tx = tx_for_handler.clone();
            let expected_app_id = app_id_for_handler.clone();
            let expected_key = key_for_handler.clone();
            async move {
                if let Event::SecretUpdate {
                    app_id,
                    key,
                    target_node_id,
                    secret,
                } = event
                {
                    assert_eq!(app_id, expected_app_id);
                    assert_eq!(key, expected_key);
                    assert!(target_node_id.is_none());
                    apply_secret_update(
                        provider.as_ref(),
                        &BootstrapKeyPair::generate(),
                        &app_id,
                        &key,
                        &secret,
                    )
                    .await
                    .unwrap();
                    if let Some(tx) = tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                }
            }
        })
        .await
        .unwrap();

        bus.publish(&Event::SecretUpdate {
            app_id: app_id.clone(),
            key: key.clone(),
            target_node_id: None,
            secret: SecretTransportEnvelope::plaintext_utf8(expected_value.clone()),
        })
        .await
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for secret update event to be handled")
            .expect("secret update handler dropped before acknowledging");

        let plaintext = provider.get(&app_id, &key).await.unwrap();
        assert_eq!(plaintext, expected_value);
        let raw = store.load_secrets(&app_id).unwrap().unwrap();
        assert_ne!(raw, expected_value.as_bytes());
    }

    #[tokio::test]
    async fn test_secret_update_event_roundtrip_persists_encrypted_targeted_secret_via_secret_provider(
    ) {
        let _nats = start_test_nats().await.unwrap();
        let bus = NatsBus::connect(&_nats.url).await.unwrap();
        bus.setup_jetstream().await.unwrap();

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = std::sync::Arc::new(LocalSecretProvider::new(
            store.clone(),
            SymmetricKey::generate(),
        ));
        let recipient = BootstrapKeyPair::generate();
        let recipient_secret_bytes = recipient.secret_bytes();
        let recipient_public_bytes = recipient.public_bytes();
        let app_id = AppId("secret-app:v1".to_string());
        let key = "API_KEY".to_string();
        let expected_value = "super-secret-over-nats-encrypted".to_string();
        let (tx, rx) = oneshot::channel();
        let provider_for_handler = provider.clone();
        let app_id_for_handler = app_id.clone();
        let key_for_handler = key.clone();
        let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let tx_for_handler = tx.clone();

        bus.subscribe(
            &format!("secrets.update.{}.node-under-test", app_id.0),
            move |event| {
                let provider = provider_for_handler.clone();
                let tx = tx_for_handler.clone();
                let expected_app_id = app_id_for_handler.clone();
                let expected_key = key_for_handler.clone();
                let recipient = BootstrapKeyPair::from_secret_bytes(recipient_secret_bytes);
                async move {
                    if let Event::SecretUpdate {
                        app_id,
                        key,
                        target_node_id,
                        secret,
                    } = event
                    {
                        assert_eq!(app_id, expected_app_id);
                        assert_eq!(key, expected_key);
                        assert_eq!(target_node_id.as_deref(), Some("node-under-test"));
                        apply_secret_update(provider.as_ref(), &recipient, &app_id, &key, &secret)
                            .await
                            .unwrap();
                        if let Some(tx) = tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                    }
                }
            },
        )
        .await
        .unwrap();

        let ciphertext =
            encrypt_for_peer(&recipient_public_bytes, expected_value.as_bytes()).unwrap();
        bus.publish(&Event::SecretUpdate {
            app_id: app_id.clone(),
            key: key.clone(),
            target_node_id: Some("node-under-test".to_string()),
            secret: SecretTransportEnvelope::node_transport_ciphertext(ciphertext),
        })
        .await
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for encrypted secret update event to be handled")
            .expect("encrypted secret update handler dropped before acknowledging");

        let plaintext = provider.get(&app_id, &key).await.unwrap();
        assert_eq!(plaintext, expected_value);
    }

    #[tokio::test]
    async fn test_fetch_artifact_sends_signed_manifest_header() {
        use axum::{
            extract::Path,
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };

        let wasm_bytes = b"artifact-manifest-header-test".to_vec();
        let expected_sha256 = hex::encode(sha2::Sha256::digest(&wasm_bytes));
        let authority = ArtifactTransferAuthority::derive("node-1", &[5u8; 32]);
        let requester_node_id = "node-2";
        let manifest =
            authority.issue_read_manifest_for_audience(&expected_sha256, requester_node_id);
        let expected_header = manifest.encode_header_value().unwrap();
        let app = Router::new().route(
            "/artifacts/{sha256}",
            get({
                let wasm_bytes = wasm_bytes.clone();
                let expected_header = expected_header.clone();
                move |Path(_sha256): Path<String>, headers: HeaderMap| {
                    let wasm_bytes = wasm_bytes.clone();
                    let expected_header = expected_header.clone();
                    async move {
                        if headers
                            .get(ARTIFACT_TRANSFER_MANIFEST_HEADER)
                            .and_then(|value| value.to_str().ok())
                            == Some(expected_header.as_str())
                            && headers
                                .get(ARTIFACT_TRANSFER_REQUESTER_NODE_HEADER)
                                .and_then(|value| value.to_str().ok())
                                == Some(requester_node_id)
                        {
                            (StatusCode::OK, wasm_bytes)
                        } else {
                            (StatusCode::FORBIDDEN, Vec::new())
                        }
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let fetched = fetch_artifact(
            &format!("http://{addr}/artifacts/{expected_sha256}"),
            Some(requester_node_id),
            Some(&manifest),
            &expected_sha256,
        )
        .await
        .unwrap();
        assert_eq!(fetched, wasm_bytes);
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_fetches_with_stored_authorization_header() {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };

        #[derive(Clone)]
        struct ArtifactState {
            expected_auth: String,
            wasm_bytes: Vec<u8>,
        }

        let wasm_bytes = b"remote-artifact-ingest-ok".to_vec();
        let sha256 = hex::encode(sha2::Sha256::digest(&wasm_bytes));
        let state = ArtifactState {
            expected_auth: "Bearer super-token".to_string(),
            wasm_bytes: wasm_bytes.clone(),
        };

        let app = Router::new()
            .route(
                "/payload.wasm",
                get(
                    |State(state): State<ArtifactState>, headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, Vec::new());
                        }
                        (StatusCode::OK, state.wasm_bytes.clone())
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        store
            .save_cluster_node(&common::types::ClusterNodeRecord::new(
                "node-under-test".to_string(),
                super::now_unix_secs(),
            ))
            .unwrap();
        store
            .save_cluster_node(&common::types::ClusterNodeRecord::new(
                "node-peer".to_string(),
                super::now_unix_secs(),
            ))
            .unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        provider
            .set(&artifact_credentials_app_id(), "ghcr-reader", "super-token")
            .await
            .unwrap();

        let response = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: None,
                url: format!("http://{addr}/payload.wasm"),
                sha256: sha256.clone(),
                credential_ref: Some("ghcr-reader".to_string()),
                signature: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(response.expected_hash, sha256);
        assert_eq!(response.size_bytes, wasm_bytes.len() as u64);
        assert_eq!(
            response.artifact_url,
            format!("http://node-under-test.internal:9091/artifacts/{sha256}")
        );
        assert_eq!(response.artifact_transfer_manifests.len(), 1);
        assert!(store.load_raw_wasm(&sha256).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_rejects_hash_mismatch() {
        use axum::{http::StatusCode, routing::get, Router};

        let app = Router::new().route(
            "/payload.wasm",
            get(|| async { (StatusCode::OK, b"wrong-bytes".to_vec()) }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());

        let err = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: None,
                url: format!("http://{addr}/payload.wasm"),
                sha256: "deadbeef".repeat(8),
                credential_ref: None,
                signature: None,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("sha256 mismatch"));
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_rejects_oversized_payload() {
        use axum::{http::StatusCode, routing::get, Router};

        let oversized_body = vec![0u8; super::MAX_REMOTE_ARTIFACT_BYTES as usize + 1];
        let app = Router::new().route(
            "/payload.wasm",
            get(move || {
                let oversized_body = oversized_body.clone();
                async move { (StatusCode::OK, oversized_body) }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());

        let err = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: None,
                url: format!("http://{addr}/payload.wasm"),
                sha256: "deadbeef".repeat(8),
                credential_ref: None,
                signature: None,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("maximum size"));
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_resolves_oci_tag_to_blob() {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };

        #[derive(Clone)]
        struct RegistryState {
            expected_auth: String,
            manifest_body: String,
            blob_bytes: Vec<u8>,
            blob_digest: String,
        }

        let blob_bytes = b"oci-registry-blob".to_vec();
        let blob_hash = hex::encode(sha2::Sha256::digest(&blob_bytes));
        let manifest_body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.unknown.config.v1+json",
                "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 2
            },
            "layers": [{
                "mediaType": "application/wasm",
                "digest": format!("sha256:{blob_hash}"),
                "size": blob_bytes.len()
            }]
        })
        .to_string();
        let state = RegistryState {
            expected_auth: "Bearer registry-token".to_string(),
            manifest_body,
            blob_bytes: blob_bytes.clone(),
            blob_digest: blob_hash.clone(),
        };

        let app = Router::new()
            .route(
                "/v2/example-org/hello-api/manifests/v1",
                get(
                    |State(state): State<RegistryState>, headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, String::new());
                        }
                        (StatusCode::OK, state.manifest_body.clone())
                    },
                ),
            )
            .route(
                "/v2/example-org/hello-api/blobs/{digest}",
                get(
                    |State(state): State<RegistryState>,
                     axum::extract::Path(digest): axum::extract::Path<String>,
                     headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, Vec::new());
                        }
                        if digest != format!("sha256:{}", state.blob_digest) {
                            return (StatusCode::NOT_FOUND, Vec::new());
                        }
                        (StatusCode::OK, state.blob_bytes.clone())
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        provider
            .set(
                &artifact_credentials_app_id(),
                "ghcr-reader",
                "authorization:Bearer registry-token",
            )
            .await
            .unwrap();

        let response = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: Some(format!(
                    "oci://127.0.0.1:{}/example-org/hello-api:v1",
                    addr.port()
                )),
                url: String::new(),
                sha256: String::new(),
                credential_ref: Some("ghcr-reader".to_string()),
                signature: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(response.expected_hash, blob_hash);
        assert_eq!(
            response.artifact_url,
            format!("http://node-under-test.internal:9091/artifacts/{blob_hash}")
        );
        assert!(store.load_raw_wasm(&blob_hash).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_selects_matching_platform_from_oci_index() {
        use axum::{
            extract::{Path, State},
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };

        #[derive(Clone)]
        struct RegistryState {
            expected_auth: String,
            index_body: String,
            matching_manifest_body: String,
            non_matching_manifest_body: String,
            matching_blob_bytes: Vec<u8>,
            matching_blob_digest: String,
            matching_manifest_digest: String,
            non_matching_manifest_digest: String,
        }

        let matching_blob_bytes = b"oci-platform-match".to_vec();
        let matching_blob_hash = hex::encode(sha2::Sha256::digest(&matching_blob_bytes));
        let matching_manifest_body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "layers": [{
                "mediaType": "application/wasm",
                "digest": format!("sha256:{matching_blob_hash}"),
                "size": matching_blob_bytes.len()
            }]
        })
        .to_string();
        let non_matching_manifest_body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "layers": [{
                "mediaType": "application/wasm",
                "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "size": 16
            }]
        })
        .to_string();
        let matching_manifest_digest =
            hex::encode(sha2::Sha256::digest(matching_manifest_body.as_bytes()));
        let non_matching_manifest_digest =
            hex::encode(sha2::Sha256::digest(non_matching_manifest_body.as_bytes()));
        let index_body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": format!("sha256:{non_matching_manifest_digest}"),
                    "size": non_matching_manifest_body.len(),
                    "platform": {
                        "os": "linux",
                        "architecture": "arm64"
                    }
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": format!("sha256:{matching_manifest_digest}"),
                    "size": matching_manifest_body.len(),
                    "platform": {
                        "os": std::env::consts::OS,
                        "architecture": normalized_host_architecture()
                    }
                }
            ]
        })
        .to_string();

        let state = RegistryState {
            expected_auth: "Bearer registry-token".to_string(),
            index_body,
            matching_manifest_body,
            non_matching_manifest_body,
            matching_blob_bytes: matching_blob_bytes.clone(),
            matching_blob_digest: matching_blob_hash.clone(),
            matching_manifest_digest: matching_manifest_digest.clone(),
            non_matching_manifest_digest: non_matching_manifest_digest.clone(),
        };

        let app = Router::new()
            .route(
                "/v2/example-org/hello-api/manifests/{reference}",
                get(
                    |State(state): State<RegistryState>,
                     Path(reference): Path<String>,
                     headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, String::new());
                        }
                        let body = if reference == "v1" {
                            state.index_body.clone()
                        } else if reference == format!("sha256:{}", state.matching_manifest_digest)
                        {
                            state.matching_manifest_body.clone()
                        } else if reference
                            == format!("sha256:{}", state.non_matching_manifest_digest)
                        {
                            state.non_matching_manifest_body.clone()
                        } else {
                            return (StatusCode::NOT_FOUND, String::new());
                        };
                        (StatusCode::OK, body)
                    },
                ),
            )
            .route(
                "/v2/example-org/hello-api/blobs/{digest}",
                get(
                    |State(state): State<RegistryState>,
                     Path(digest): Path<String>,
                     headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, Vec::new());
                        }
                        if digest != format!("sha256:{}", state.matching_blob_digest) {
                            return (StatusCode::NOT_FOUND, Vec::new());
                        }
                        (StatusCode::OK, state.matching_blob_bytes.clone())
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        provider
            .set(
                &artifact_credentials_app_id(),
                "ghcr-reader",
                "authorization:Bearer registry-token",
            )
            .await
            .unwrap();

        let response = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: Some(format!(
                    "oci://127.0.0.1:{}/example-org/hello-api:v1",
                    addr.port()
                )),
                url: String::new(),
                sha256: String::new(),
                credential_ref: Some("ghcr-reader".to_string()),
                signature: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(response.expected_hash, matching_blob_hash);
        assert!(store.load_raw_wasm(&matching_blob_hash).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_apply_secret_update_rejects_bootstrap_payload_for_rotation() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        let app_id = AppId("secret-app:v1".to_string());

        let err = apply_secret_update(
            &provider,
            &BootstrapKeyPair::generate(),
            &app_id,
            "API_KEY",
            &SecretTransportEnvelope::bootstrap_peer_ciphertext(vec![0xff, 0xfe, 0xfd]),
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("unexpected secret payload variant"));
        assert!(store.load_secrets(&app_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn test_handle_state_snapshot_accepts_first_matching_session_only() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let bootstrap_session = Arc::new(Mutex::new(BootstrapSessionState {
            session_id: "session-1".to_string(),
            nonce: "nonce-1".to_string(),
            keypair: secrets::BootstrapKeyPair::generate(),
            applied: false,
        }));
        let dispatcher =
            build_test_dispatcher(store.clone(), Some(bootstrap_session.clone()), None).await;

        let stale_config = common::types::AppConfig {
            id: AppId("stale-app:v1".to_string()),
            fuel_quota: common::types::FuelQuota(1000),
            memory_limit: common::types::MemoryPages(4),
            max_instances: 1,
            idle_timeout_secs: 30,
            wasm_bind_port: 8080,
            env_vars: std::collections::HashMap::new(),
            secret_keys: vec![],
            extended_limits: None,
            health_check_path: None,
            db_max_connections: None,
            rate_limit: None,
            tenant_id: None,
            policy: None,
            namespace: "default".to_string(),
        };
        let accepted_config = common::types::AppConfig {
            id: AppId("accepted-app:v1".to_string()),
            ..stale_config.clone()
        };
        let duplicate_config = common::types::AppConfig {
            id: AppId("duplicate-app:v1".to_string()),
            ..stale_config.clone()
        };

        dispatcher
            .handle_state_snapshot(
                "stale-session".to_string(),
                "stale-nonce".to_string(),
                vec![stale_config.clone()],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(
            store.load_config(&stale_config.id).unwrap().is_none(),
            "mismatched session/nonce snapshot must be ignored"
        );

        dispatcher
            .handle_state_snapshot(
                "session-1".to_string(),
                "nonce-1".to_string(),
                vec![accepted_config.clone()],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(
            store.load_config(&accepted_config.id).unwrap().is_some(),
            "first matching snapshot should be applied"
        );

        dispatcher
            .handle_state_snapshot(
                "session-1".to_string(),
                "nonce-1".to_string(),
                vec![duplicate_config.clone()],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(
            store.load_config(&duplicate_config.id).unwrap().is_none(),
            "duplicate matching snapshot after apply must be ignored"
        );

        let bootstrap_state = bootstrap_session.lock().await;
        assert!(bootstrap_state.applied);
    }

    #[tokio::test]
    async fn test_route_webhook_uses_peer_ips_from_node_load_updates() {
        use axum::{
            extract::{Json, State},
            http::{HeaderMap, StatusCode},
            routing::post,
            Router,
        };
        use proxy::dns_webhook::RouteChangeWebhook;
        use std::sync::Arc as StdArc;
        use tokio::sync::oneshot;

        type WebhookCaptureSender =
            StdArc<std::sync::Mutex<Option<oneshot::Sender<(HeaderMap, RouteChangeWebhook)>>>>;

        #[derive(Clone)]
        struct WebhookState {
            sender: WebhookCaptureSender,
        }

        let (tx, rx) = oneshot::channel();
        let state = WebhookState {
            sender: StdArc::new(std::sync::Mutex::new(Some(tx))),
        };

        let app = Router::new()
            .route(
                "/dns",
                post(
                    |State(state): State<WebhookState>,
                     headers: HeaderMap,
                     Json(payload): Json<RouteChangeWebhook>| async move {
                        if let Some(tx) = state.sender.lock().unwrap().take() {
                            let _ = tx.send((headers, payload));
                        }
                        StatusCode::OK
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let webhook = proxy::dns_webhook::DnsWebhookManager::new(
            Some(format!("http://{addr}/dns")),
            Some("test-token".to_string()),
        )
        .unwrap();
        let dispatcher = build_test_dispatcher(store, None, Some(webhook)).await;

        dispatcher
            .handle(Event::NodeLoad {
                node_id: "node-remote".to_string(),
                cpu_percent: 10.0,
                fuel_budget_used_percent: 25.0,
                active_instances: 2,
                proxy_address: "10.0.0.42:8080".to_string(),
            })
            .await
            .unwrap();

        dispatcher
            .handle(Event::RouteAdd {
                route: common::types::Route {
                    host: "hello.example.com".to_string(),
                    app_id: AppId("hello:v1".to_string()),
                    path_prefix: "/".to_string(),
                    strip_prefix: false,
                    created_at: 1,
                    updated_at: 1,
                },
            })
            .await
            .unwrap();

        let (headers, payload) = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for DNS webhook")
            .expect("DNS webhook sender dropped");

        assert_eq!(
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer test-token")
        );
        assert_eq!(payload.action, "add");
        assert_eq!(payload.hostname, "hello.example.com");
        assert_eq!(payload.app_id, "hello:v1");
        assert_eq!(payload.node_ips, vec!["10.0.0.42".to_string()]);
    }
}
