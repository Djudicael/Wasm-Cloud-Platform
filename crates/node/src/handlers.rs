use common::{error::PlatformError, types::AppId};
use messaging::events::Event;
use proxy::dns_webhook::DnsWebhookManager;
use proxy::node_table::NodeLoadTable;
use proxy::router::HostRouter;
use proxy::upstream::UpstreamRegistry;
use reqwest::Url;
use runtime::WasmRuntime;
use secrets::{encrypt_for_peer, BootstrapKeyPair, SecretProvider};
use sha2::Digest;
use std::net::IpAddr;
use std::sync::Arc;
use storage::Store;
use supervisor::Supervisor;
use tracing::{error, info, warn};

fn is_loopback_artifact_url(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .map(|host| {
            let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
            trimmed.eq_ignore_ascii_case("localhost")
                || trimmed
                    .parse::<IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn validate_peer_artifact_url(
    new_node_id: &str,
    peer_artifact_url: &str,
) -> Result<(), PlatformError> {
    if is_loopback_artifact_url(peer_artifact_url) {
        return Err(PlatformError::config_validation(format!(
            "node {} advertised loopback artifact URL {} which is invalid for remote cluster exchange",
            new_node_id, peer_artifact_url
        )));
    }
    Ok(())
}

async fn apply_secret_update<S: SecretProvider + ?Sized>(
    secret_provider: &S,
    app_id: &AppId,
    key: &str,
    secret_bytes: &[u8],
) -> Result<(), PlatformError> {
    // Current control-plane behavior sends UTF-8 plaintext bytes in SecretUpdate.
    // Normalize those bytes through the SecretProvider so secrets are stored in the
    // provider's canonical bundle format instead of raw bytes being written to redb.
    //
    // When cluster-key encryption is implemented, this function is the correct place
    // to decrypt the incoming payload first and then call `secret_provider.set(...)`.
    let plaintext = String::from_utf8(secret_bytes.to_vec()).map_err(|e| {
        PlatformError::encryption_with_msg("secret update payload is not valid UTF-8 plaintext", e)
    })?;
    secret_provider.set(app_id, key, &plaintext).await
}

pub struct EventDispatcher {
    pub supervisor: Arc<Supervisor>,
    pub upstream: Arc<UpstreamRegistry>,
    pub host_router: Arc<HostRouter>,
    pub store: Store,
    pub runtime: WasmRuntime,
    pub node_id: String,
    pub artifact_server_url: String,
    pub upgrade_signing_public_key: Option<String>,
    pub supervisor_addr: std::net::SocketAddr,
    pub secret_provider: Arc<dyn SecretProvider>,
    pub bootstrap_keypair: Option<BootstrapKeyPair>,
    pub bus: messaging::NatsBus,
    pub dns_webhook: Option<DnsWebhookManager>,
    pub node_table: Arc<NodeLoadTable>,
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
                artifact_auth_token,
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
                    artifact_auth_token,
                    expected_hash,
                    size_bytes,
                )
                .await
            }
            Event::RemoveApp { app_id } => self.handle_remove(app_id).await,
            Event::RouteAdd { route } => {
                self.store.save_route(&route)?;
                self.host_router
                    .add_route(
                        route.host.clone(),
                        route.path_prefix.clone(),
                        route.app_id.clone(),
                        route.strip_prefix,
                    )
                    .await;
                info!(host = %route.host, app = %route.app_id.0, "route added");
                if let Some(ref webhook) = self.dns_webhook {
                    webhook
                        .notify_route_change("add", &route.host, &route.app_id.0)
                        .await;
                }
                Ok(())
            }
            Event::RouteRemove { host } => {
                // Load route to get app_id and path_prefix for webhook before deleting.
                // A missing pre-delete lookup is not fatal — it only affects webhook context.
                let existing = self.store.load_route(&host).ok().flatten();
                let app_id = existing.as_ref().map(|r| r.app_id.clone());
                let path_prefix = existing
                    .as_ref()
                    .map(|r| r.path_prefix.clone())
                    .unwrap_or_default();
                self.store.delete_route(&host)?;
                self.host_router.remove_route(&host, &path_prefix).await;
                info!(host, "route removed");
                if let Some(ref webhook) = self.dns_webhook {
                    if let Some(app_id) = app_id {
                        webhook
                            .notify_route_change("remove", &host, &app_id.0)
                            .await;
                    }
                }
                Ok(())
            }
            Event::InstanceReady {
                app_id,
                addr,
                node_id,
            } => {
                // Only register if it's from a DIFFERENT node
                // (our own instances are registered directly by the Supervisor)
                if node_id != self.our_node_id() {
                    self.upstream.add(&app_id, addr).await;
                    info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance registered");
                }
                Ok(())
            }
            Event::InstanceDead {
                app_id,
                addr,
                node_id,
            } => {
                if node_id != self.our_node_id() {
                    self.upstream.remove(&app_id, &addr).await;
                    info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance deregistered");
                }
                Ok(())
            }
            Event::SecretUpdate {
                app_id,
                key,
                encrypted_value,
            } => {
                info!(app = %app_id.0, key, "received secret rotation");
                apply_secret_update(
                    self.secret_provider.as_ref(),
                    &app_id,
                    &key,
                    &encrypted_value,
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
            } => {
                // Update node table for cross-node routing decisions
                use proxy::node_table::NodeEntry;
                use std::time::{SystemTime, UNIX_EPOCH};
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let entry = NodeEntry {
                    node_id: node_id.clone(),
                    supervisor_addr: self.supervisor_addr,
                    fuel_used_percent: fuel_budget_used_percent,
                    active_instances,
                    last_seen: now,
                    health_status: common::health::NodeHealthStatus::Healthy,
                };
                self.node_table.update(entry).await;

                // Update DNS webhook with node IPs for webhook notifications
                if let Some(ref webhook) = self.dns_webhook {
                    let nodes = self.node_table.nodes.read().await;
                    let ips: Vec<String> = nodes
                        .values()
                        .map(|n| n.supervisor_addr.ip().to_string())
                        .collect();
                    drop(nodes);
                    webhook.set_node_ips(ips).await;
                }
                Ok(())
            }
            Event::NodeJoined {
                node_id,
                artifact_server_url,
                artifact_auth_token,
                public_key_bytes,
                protocol_version,
                binary_version,
            } => {
                self.handle_node_joined(
                    node_id,
                    artifact_server_url,
                    artifact_auth_token,
                    public_key_bytes,
                    protocol_version,
                    binary_version,
                )
                .await
            }
            Event::StateSnapshot {
                for_node_id,
                configs,
                routes,
                encrypted_secrets,
                artifact_hashes,
            } => {
                if for_node_id == self.node_id {
                    self.handle_state_snapshot(configs, routes, encrypted_secrets, artifact_hashes)
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
                if node_id == self.node_id {
                    // Our own pressure events are handled directly by the
                    // eBPF ActionDispatcher — no additional action needed here.
                } else {
                    // A peer node is under pressure — stop steering traffic to it.
                    warn!(
                        node = %node_id,
                        pressure_level,
                        "peer node under pressure — removing from routing"
                    );
                    self.node_table.mark_unhealthy(&node_id).await;
                    if pressure_level >= 2 {
                        // Critical pressure: also remove all upstream entries
                        tracing::warn!(
                            node = %node_id,
                            "peer node under CRITICAL pressure — removing all upstream entries"
                        );
                    }
                }
                Ok(())
            }

            Event::NodePressureRecovered { node_id } => {
                if node_id == self.node_id {
                    info!("our node pressure recovered");
                } else {
                    info!(node = %node_id, "peer node pressure recovered — restoring in routing");
                    self.node_table.mark_healthy(&node_id).await;
                }
                Ok(())
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
                // Update our node table with the new health status
                let health_status = match status.as_str() {
                    "healthy" => common::health::NodeHealthStatus::Healthy,
                    "degraded" => common::health::NodeHealthStatus::Degraded,
                    "unhealthy" => common::health::NodeHealthStatus::Unhealthy,
                    _ => common::health::NodeHealthStatus::Degraded,
                };
                self.node_table.update_health(&node_id, health_status).await;
                Ok(())
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
                // Update node table health status from snapshot
                let health_status = match status.as_str() {
                    "healthy" => common::health::NodeHealthStatus::Healthy,
                    "degraded" => common::health::NodeHealthStatus::Degraded,
                    "unhealthy" => common::health::NodeHealthStatus::Unhealthy,
                    _ => common::health::NodeHealthStatus::Degraded,
                };
                self.node_table.update_health(&node_id, health_status).await;
                Ok(())
            }

            Event::GatewayConfigUpdate { app_id, config } => {
                info!(app = %app_id.0, "received gateway config update");
                self.store.save_gateway_config(&app_id.0, &config)?;
                // Keep the in-memory gateway cache in sync so the internal
                // proxy can enforce endpoint policies without reloading from disk.
                if let Some(ref gw) = self.gateway {
                    gw.set_route_config(&app_id.0, config).await;
                }
                Ok(())
            }
            Event::GatewayConfigRemove { app_id } => {
                info!(app = %app_id.0, "received gateway config remove");
                self.store.delete_gateway_config(&app_id.0)?;
                if let Some(ref gw) = self.gateway {
                    gw.remove_route_config(&app_id.0).await;
                }
                Ok(())
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
        }
    }

    async fn handle_deploy(
        &self,
        app_id: AppId,
        config: common::types::AppConfig,
        artifact_url: String,
        artifact_auth_token: Option<String>,
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
            let bytes = fetch_artifact(&artifact_url, artifact_auth_token.as_deref(), &sha256)
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

    async fn handle_node_joined(
        &self,
        new_node_id: String,
        peer_artifact_url: String,
        peer_artifact_token: Option<String>,
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

        if new_node_id != self.node_id {
            validate_peer_artifact_url(&new_node_id, &peer_artifact_url)?;
            if peer_artifact_token
                .as_deref()
                .map(str::is_empty)
                .unwrap_or(true)
            {
                return Err(PlatformError::config_validation(format!(
                    "node {} advertised remote artifact URL {} without an artifact auth token",
                    new_node_id, peer_artifact_url
                )));
            }
        }

        // Leader election: only nodes with IDs smaller than the new node respond.
        // This means multiple existing nodes could respond if they all have smaller IDs.
        // In practice, the new node should accept the first valid snapshot it receives
        // and ignore subsequent responses. Ideally, only the smallest existing node
        // would respond, but without knowing all node IDs, we use this simpler approach.
        if self.node_id > new_node_id {
            return Ok(()); // A smaller node should respond instead
        }

        info!(
            new_node = %new_node_id,
            our_node = %self.node_id,
            "sending state snapshot to new node"
        );

        // 1. Collect all configs
        let app_ids = self.store.list_apps()?;
        let mut configs = Vec::with_capacity(app_ids.len());
        for id in &app_ids {
            if let Some(config) = self.store.load_config(id)? {
                configs.push(config);
            }
        }

        // 2. Collect all routes
        let routes = self.store.list_routes()?;

        // 3. Encrypt secrets for each app
        let mut encrypted_secrets = Vec::new();
        for config in &configs {
            let keys = self.secret_provider.list_keys(&config.id).await?;
            for key in keys {
                let value = self.secret_provider.get(&config.id, &key).await?;
                let encrypted = encrypt_for_peer(&peer_public_key, value.as_bytes())?;
                encrypted_secrets.push((config.id.0.clone(), key, encrypted));
            }
        }

        // 4. Collect artifact hashes
        let mut artifact_hashes = Vec::new();
        for config in &configs {
            if let Some(hash) = self.store.get_artifact_sha256(&config.id)? {
                artifact_hashes.push((config.id.0.clone(), hash));
            }
        }

        // 5. Publish the snapshot event
        let event = Event::StateSnapshot {
            for_node_id: new_node_id.clone(),
            configs,
            routes,
            encrypted_secrets,
            artifact_hashes: artifact_hashes.clone(),
        };

        // Publish via NATS (we need access to the bus - will fix in main.rs)
        info!(
            new_node = %new_node_id,
            apps = artifact_hashes.len(),
            "snapshot prepared"
        );

        // 6. Push artifacts to new node in background
        let store = self.store.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            for (app_id_str, sha256) in &artifact_hashes {
                if let Ok(Some(raw)) = store.load_raw_wasm(sha256) {
                    let url = format!("{peer_artifact_url}/artifacts/{sha256}");
                    let mut request = client.put(&url).body(raw);
                    if let Some(token) = peer_artifact_token.as_deref() {
                        request = request.bearer_auth(token);
                    }
                    match request.send().await {
                        Ok(resp) if resp.status().is_success() => {
                            info!(app = app_id_str, sha256, "pushed artifact to new node");
                        }
                        Ok(resp) => {
                            warn!(
                                app = app_id_str,
                                sha256,
                                status = %resp.status(),
                                "failed to push artifact"
                            );
                        }
                        Err(e) => {
                            warn!(app = app_id_str, sha256, error = %e, "artifact push error");
                        }
                    }
                }
            }
        });

        // Publish the snapshot event via NATS
        self.bus.publish(&event).await?;
        Ok(())
    }

    async fn handle_state_snapshot(
        &self,
        configs: Vec<common::types::AppConfig>,
        routes: Vec<common::types::Route>,
        encrypted_secrets: Vec<(String, String, Vec<u8>)>,
        artifact_hashes: Vec<(String, String)>,
    ) -> Result<(), PlatformError> {
        info!(
            apps = configs.len(),
            routes = routes.len(),
            secrets = encrypted_secrets.len(),
            "received state snapshot"
        );

        // 1. Store configs
        for config in &configs {
            self.store.save_config(config)?;
        }

        // 2. Store routes and load into HostRouter
        for route in &routes {
            self.store.save_route(route)?;
            self.host_router
                .add_route(
                    route.host.clone(),
                    route.path_prefix.clone(),
                    route.app_id.clone(),
                    route.strip_prefix,
                )
                .await;
        }

        // 3. Decrypt and store secrets
        let keypair = self.bootstrap_keypair.as_ref().ok_or_else(|| {
            PlatformError::encryption("no bootstrap keypair available to decrypt secrets")
        })?;

        for (app_id_str, key, encrypted_value) in encrypted_secrets {
            let app_id = AppId(app_id_str.clone());

            // Decrypt using our bootstrap keypair
            let plaintext_bytes = keypair.decrypt(&encrypted_value)?;
            let plaintext = String::from_utf8(plaintext_bytes)
                .map_err(|e| PlatformError::encryption_with_msg("secret not valid UTF-8", e))?;
            self.secret_provider.set(&app_id, &key, &plaintext).await?;
            info!(app = app_id_str, key, "secret decrypted and stored");
        }

        // 4. Store artifact hashes
        for (app_id_str, sha256) in &artifact_hashes {
            let app_id = AppId(app_id_str.clone());
            self.store.save_artifact_hash(&app_id, sha256)?;
        }

        // 5. Compile artifacts (artifacts should already be in our local store from push)
        for (app_id_str, sha256) in artifact_hashes {
            let app_id = AppId(app_id_str.clone());

            // Wait for the artifact to arrive via HTTP push with a retry loop.
            // The peer node pushes artifacts asynchronously, so we may need to
            // wait for the HTTP PUT to complete before we can compile.
            let artifact = {
                let mut attempts = 0;
                loop {
                    if let Ok(Some(raw)) = self.store.load_raw_wasm(&sha256) {
                        break Some(raw);
                    }
                    if attempts >= 50 {
                        // 5 seconds total (50 * 100ms)
                        break None;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    attempts += 1;
                }
            };

            if let Some(raw) = artifact {
                let runtime = self.runtime.clone();
                let store = self.store.clone();
                let app_id_clone = app_id.clone();

                tokio::task::spawn_blocking(move || match runtime.compile(&raw) {
                    Ok(compiled) => {
                        if let Err(e) = store.store_artifact(&app_id_clone, &compiled) {
                            error!(app = %app_id_str, error = %e, "failed to store compiled artifact");
                        } else {
                            info!(app = %app_id_str, "artifact compiled from snapshot");
                        }
                    }
                    Err(e) => {
                        error!(app = %app_id_str, error = %e, "compilation failed");
                    }
                });
            } else {
                warn!(
                    app = app_id_str,
                    sha256, "artifact not yet available, will compile on first request"
                );
            }
        }

        info!("state snapshot import complete");
        Ok(())
    }

    async fn handle_node_upgrade(&self, event: Event) {
        use crate::upgrade::{
            download_and_verify, handle_upgrade_event, verify_upgrade_signature, UpgradeAction,
        };

        // Collect all node IDs in the cluster for rolling upgrade ordering
        let cluster_nodes = {
            // TODO: Maintain a proper node registry from NodeJoined/NodeLoad events
            // For now, use the node load table which tracks known cluster nodes
            let nodes = self.node_table.nodes.read().await;
            let mut ids: Vec<String> = nodes.keys().cloned().collect();
            // Always include our own node ID in case it's not in the table yet
            if !ids.contains(&self.node_id) {
                ids.push(self.node_id.clone());
            }
            ids
        };

        if let Err(e) = verify_upgrade_signature(&event, self.upgrade_signing_public_key.as_deref())
        {
            error!(error = %e, "upgrade signature verification failed");
            return;
        }

        match handle_upgrade_event(&event, &self.node_id, &cluster_nodes) {
            Ok(UpgradeAction::NotAnUpgradeEvent) => {
                warn!("handle_node_upgrade called with non-upgrade event");
            }
            Ok(UpgradeAction::NotTargeted) => {
                info!("upgrade not targeted at this node");
            }
            Ok(UpgradeAction::WaitForPredecessor { predecessor }) => {
                info!(
                    predecessor,
                    "waiting for predecessor node to complete upgrade"
                );
                // Store upgrade intent and wait for NodeUpgradeComplete event
            }
            Ok(UpgradeAction::IncompatibleVersion) => {
                error!("upgrade version is incompatible with this node's protocol version");
            }
            Ok(UpgradeAction::ProceedWithUpgrade) => {
                if let Event::NodeUpgrade {
                    binary_url,
                    binary_sha256,
                    new_protocol_version,
                    new_binary_version,
                    ..
                } = event
                {
                    info!(
                        url = %binary_url,
                        version = %new_binary_version,
                        protocol = new_protocol_version,
                        "proceeding with upgrade"
                    );

                    // Download and verify the new binary
                    let install_dir = std::path::PathBuf::from("/opt/wasm-cloud");
                    match download_and_verify(&binary_url, &binary_sha256, &install_dir, "node")
                        .await
                    {
                        Ok(new_binary_path) => {
                            info!(path = ?new_binary_path, "new binary downloaded, verified, and activated");

                            info!("release links updated, initiating graceful shutdown");

                            // Publish draining event
                            let drain_event = Event::NodeDraining {
                                node_id: self.node_id.clone(),
                                drain_timeout_secs: 30,
                            };

                            if let Err(e) = self.bus.publish(&drain_event).await {
                                error!(error = %e, "failed to publish draining event");
                            }

                            // Begin graceful shutdown
                            self.begin_graceful_shutdown(30).await;

                            // Publish upgrade complete event
                            let complete_event = Event::NodeUpgradeComplete {
                                node_id: self.node_id.clone(),
                                new_binary_version,
                                new_protocol_version,
                            };

                            if let Err(e) = self.bus.publish(&complete_event).await {
                                error!(error = %e, "failed to publish upgrade complete event");
                            }

                            // Exit process - systemd will restart with new binary
                            info!("exiting for upgrade, expecting systemd restart");
                            std::process::exit(0);
                        }
                        Err(e) => {
                            error!(error = %e, "failed to download or verify new binary");
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "upgrade event handling failed");
            }
        }
    }

    async fn begin_graceful_shutdown(&self, timeout_secs: u64) {
        tracing::info!("Beginning graceful shutdown with {}s timeout", timeout_secs);

        // 1. Stop accepting new connections via backpressure signal
        //    TODO: Add backpressure_signal field to EventDispatcher so we can
        //    call backpressure.set_rejecting() here. For now, the proxy will
        //    continue accepting connections until the process exits.

        // 2. Wait for existing requests to drain
        tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs)).await;

        // 3. Kill all running instances
        tracing::info!("drain timeout elapsed, stopping all instances");
        self.supervisor
            .shutdown_all(tokio::time::Duration::from_secs(timeout_secs))
            .await;

        tracing::info!("graceful shutdown complete");
    }
}

/// Fetch an artifact from a URL and verify its SHA-256 hash.
async fn fetch_artifact(
    url: &str,
    artifact_auth_token: Option<&str>,
    expected_sha256: &str,
) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::new();
    let mut request = client.get(url);
    if let Some(token) = artifact_auth_token {
        request = request.bearer_auth(token);
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
    use super::{apply_secret_update, is_loopback_artifact_url, validate_peer_artifact_url};
    use common::types::AppId;
    use secrets::{crypto::SymmetricKey, LocalSecretProvider, SecretProvider};
    use storage::Store;
    use tempfile::NamedTempFile;

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

        apply_secret_update(&provider, &app_id, "API_KEY", b"super-secret-value")
            .await
            .unwrap();

        let plaintext = provider.get(&app_id, "API_KEY").await.unwrap();
        assert_eq!(plaintext, "super-secret-value");

        let raw = store.load_secrets(&app_id).unwrap().unwrap();
        assert_ne!(raw, b"super-secret-value");
    }

    #[tokio::test]
    async fn test_apply_secret_update_rejects_non_utf8_plaintext_payload() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        let app_id = AppId("secret-app:v1".to_string());

        let err = apply_secret_update(&provider, &app_id, "API_KEY", &[0xff, 0xfe, 0xfd])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("valid UTF-8 plaintext"));
        assert!(store.load_secrets(&app_id).unwrap().is_none());
    }
}
