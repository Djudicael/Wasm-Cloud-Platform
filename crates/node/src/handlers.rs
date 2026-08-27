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
#[cfg(test)]
mod tests;
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
                self.handle_secret_update(app_id, key, target_node_id, secret)
                    .await
            }
            Event::ConfigUpdate { app_id, config } => self.handle_config_update(app_id, config),
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
                self.handle_node_upgrade_complete(node_id, new_binary_version, new_protocol_version)
            }
            Event::NodeDraining {
                node_id,
                drain_timeout_secs,
            } => self.handle_node_draining(node_id, drain_timeout_secs).await,

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
            } => self.handle_security_incident(node_id, app_id, pid, syscall_nr, category),

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
                self.handle_config_hot_reload(node_id, changes)
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
            let Some(targeted_manifest) = artifact_transfer_manifests
                .iter()
                .find(|binding| binding.audience_node_id == self.node_id)
                .map(|binding| &binding.artifact_transfer_manifest)
            else {
                // Durable consumers replay historical deployments when a new node joins.
                // A missing audience binding means that this node was not active when the
                // artifact was published, so it was not an intended transfer target. NAKing
                // cannot make an authorization appear and can eventually stall later control
                // events behind an unprocessable message.
                info!(
                    app = %app_id.0,
                    artifact = %sha256,
                    node = %self.node_id,
                    "skipping deploy event not authorized for this node"
                );
                return Ok(());
            };
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
        self.store.mark_deployed(&app_id.0)?;
        info!(
            app = %app_id.0,
            "deploy complete, waiting for first request"
        );
        Ok(())
    }

    async fn handle_remove(&self, app_id: AppId) -> Result<(), PlatformError> {
        info!(app = %app_id.0, "removing app");

        // Remove routes before stopping instances so new requests cannot race
        // the undeploy and cold-start an application being removed.
        let route_hosts: std::collections::BTreeSet<_> = self
            .store
            .list_routes_for_app(&app_id.0)?
            .into_iter()
            .map(|route| route.host)
            .collect();
        for host in route_hosts {
            self.handle_route_remove(host).await?;
        }

        // Kill all running instances first (this creates billing records)
        self.supervisor.kill_all_instances(&app_id).await?;

        // Mark app as undeployed - starts grace period
        // Actual deletion happens after grace period expires in GC loop
        self.store.mark_undeployed(&app_id.0)?;
        self.supervisor.forget_app(&app_id).await?;

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
            // Runtime instances bind to the guest loopback interface. A loopback
            // address announced by another node refers to that peer's namespace,
            // not ours; registering it locally can collide with a local instance
            // using the same port and route traffic to the wrong protocol socket.
            // Cross-node steering uses the peer's advertised node proxy instead.
            if addr.ip().is_loopback() {
                info!(
                    app = %app_id.0,
                    %addr,
                    from_node = %node_id,
                    "ignoring peer-local instance address"
                );
                return Ok(());
            }
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
            if addr.ip().is_loopback() {
                return Ok(());
            }
            self.upstream.remove(&app_id, &addr).await;
            info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance deregistered");
        }
        Ok(())
    }

    async fn handle_secret_update(
        &self,
        app_id: AppId,
        key: String,
        target_node_id: Option<String>,
        secret: secrets::SecretTransportEnvelope,
    ) -> Result<(), PlatformError> {
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
        .await
    }

    fn handle_config_update(
        &self,
        app_id: AppId,
        config: common::types::AppConfig,
    ) -> Result<(), PlatformError> {
        self.store.save_config(&config)?;
        info!(app = %app_id.0, "config updated");
        Ok(())
    }

    fn handle_node_upgrade_complete(
        &self,
        node_id: String,
        new_binary_version: String,
        new_protocol_version: u32,
    ) -> Result<(), PlatformError> {
        info!(
            node = %node_id,
            version = %new_binary_version,
            protocol = new_protocol_version,
            "node upgrade completed"
        );
        Ok(())
    }

    async fn handle_node_draining(
        &self,
        node_id: String,
        drain_timeout_secs: u64,
    ) -> Result<(), PlatformError> {
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

    fn handle_security_incident(
        &self,
        node_id: String,
        app_id: String,
        pid: u32,
        syscall_nr: u64,
        category: String,
    ) -> Result<(), PlatformError> {
        tracing::error!(
            node = %node_id,
            app = %app_id,
            pid,
            syscall_nr,
            category,
            "SECURITY INCIDENT: privileged syscall detected by eBPF monitor"
        );
        if node_id != self.node_id {
            warn!(
                node = %node_id,
                app = %app_id,
                "security incident on peer node — consider quarantining artifact"
            );
        }
        Ok(())
    }

    fn handle_config_hot_reload(
        &self,
        node_id: String,
        changes: serde_json::Value,
    ) -> Result<(), PlatformError> {
        tracing::info!(
            node = %node_id,
            changes = ?changes,
            "peer node changed hot-reloadable config (informational only, not auto-applied)"
        );
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
