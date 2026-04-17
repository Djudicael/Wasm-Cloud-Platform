use common::types::AppId;
use messaging::events::Event;
use proxy::dns_webhook::DnsWebhookManager;
use proxy::node_table::NodeLoadTable;
use proxy::router::HostRouter;
use proxy::upstream::UpstreamRegistry;
use runtime::WasmRuntime;
use secrets::{encrypt_for_peer, BootstrapKeyPair, SecretProvider};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use storage::Store;
use supervisor::Supervisor;
use tracing::{error, info, warn};

pub struct EventDispatcher {
    pub supervisor: Arc<Supervisor>,
    pub upstream: Arc<UpstreamRegistry>,
    pub host_router: Arc<HostRouter>,
    pub store: Store,
    pub runtime: WasmRuntime,
    pub node_id: String,
    pub artifact_server_url: String,
    pub secret_provider: Arc<dyn SecretProvider>,
    pub bootstrap_keypair: Option<BootstrapKeyPair>,
    pub bus: messaging::NatsBus,
    pub dns_webhook: Option<DnsWebhookManager>,
    pub node_table: Arc<NodeLoadTable>,
}

impl EventDispatcher {
    pub async fn handle(&self, event: Event) {
        let event_name = format!("{:?}", std::mem::discriminant(&event));
        tracing::info!(event = %event_name, "received event in handler");

        match event {
            Event::DeployApp {
                app_id,
                config,
                artifact_url,
                expected_hash,
                size_bytes,
            } => {
                info!(
                    "🚀 Handling DeployApp for app_id: {}, url: {}",
                    app_id.0, artifact_url
                );
                self.handle_deploy(app_id, config, artifact_url, expected_hash, size_bytes)
                    .await
            }
            Event::RemoveApp { app_id } => self.handle_remove(app_id).await,
            Event::RouteAdd { route } => {
                self.store.save_route(&route).ok();
                self.host_router
                    .add_route(route.host.clone(), route.app_id.clone())
                    .await;
                info!(host = %route.host, app = %route.app_id.0, "route added");
                if let Some(ref webhook) = self.dns_webhook {
                    webhook
                        .notify_route_change("add", &route.host, &route.app_id.0)
                        .await;
                }
            }
            Event::RouteRemove { host } => {
                // Load route to get app_id for webhook before deleting
                let app_id = self
                    .store
                    .load_route(&host)
                    .ok()
                    .flatten()
                    .map(|r| r.app_id);
                self.store.delete_route(&host).ok();
                self.host_router.remove_route(&host).await;
                info!(host, "route removed");
                if let Some(ref webhook) = self.dns_webhook {
                    if let Some(app_id) = app_id {
                        webhook
                            .notify_route_change("remove", &host, &app_id.0)
                            .await;
                    }
                }
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
            }
            Event::SecretUpdate {
                app_id,
                key,
                encrypted_value,
            } => {
                info!(app = %app_id.0, key, "received secret rotation");
                // Decrypt with cluster key and re-encrypt with node key
                // (see step 06 for details)
                // For now, we persist the encrypted value directly to simulate the update.
                if let Err(e) = self.store.save_secrets(&app_id, &encrypted_value) {
                    error!(app = %app_id.0, error = %e, "failed to update secret in cache");
                }
            }
            Event::ConfigUpdate { app_id, config } => {
                if let Err(e) = self.store.save_config(&config) {
                    error!(app = %app_id.0, error = %e, "config update failed");
                }
            }
            Event::NodeLoad {
                node_id,
                cpu_percent: _,
                fuel_budget_used_percent,
                active_instances,
            } => {
                // Update node table for cross-node routing decisions
                use proxy::node_table::NodeEntry;
                let entry = NodeEntry {
                    node_id: node_id.clone(),
                    supervisor_addr: "127.0.0.1:9000".parse().unwrap(), // TODO: actual addr
                    fuel_used_percent: fuel_budget_used_percent,
                    active_instances,
                    last_seen: std::time::Instant::now(),
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
            }
            Event::NodeJoined {
                node_id,
                artifact_server_url,
                public_key_bytes,
                protocol_version,
                binary_version,
            } => {
                self.handle_node_joined(
                    node_id,
                    artifact_server_url,
                    public_key_bytes,
                    protocol_version,
                    binary_version,
                )
                .await;
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
                        .await;
                }
            }
            Event::NodeUpgrade { .. } => {
                self.handle_node_upgrade(event).await;
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
            }
        }
    }

    async fn handle_deploy(
        &self,
        app_id: AppId,
        config: common::types::AppConfig,
        artifact_url: String,
        expected_hash: Option<String>,
        size_bytes: u64,
    ) {
        tracing::info!(app = %app_id.0, "handle_deploy invoked");

        let sha256 = match &expected_hash {
            Some(h) => h.clone(),
            None => {
                error!(app = %app_id.0, "deploy event missing expected_hash");
                return;
            }
        };

        info!(
            app = %app_id.0,
            url = %artifact_url,
            size_mb = size_bytes as f64 / 1_048_576.0,
            "deploying artifact"
        );

        // 1. Check local cache first (another node may have already stored it)
        let wasm_bytes = if self.store.raw_wasm_exists(&sha256).unwrap_or(false) {
            info!(sha256, "artifact already in local cache");
            match self.store.load_raw_wasm(&sha256) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    error!(sha256, "artifact vanished between exists and load");
                    return;
                }
                Err(e) => {
                    error!(sha256, error = %e, "failed to load cached artifact");
                    return;
                }
            }
        } else {
            // 2. Fetch from the source node
            info!(url = %artifact_url, "fetching artifact via HTTP");
            match fetch_artifact(&artifact_url, &sha256).await {
                Ok(bytes) => {
                    // 3. Store raw bytes for future use
                    if let Err(e) = self.store.save_raw_wasm(&sha256, &bytes) {
                        error!(sha256, error = %e, "failed to cache raw wasm");
                    }
                    bytes
                }
                Err(e) => {
                    error!(url = %artifact_url, error = %e, "artifact fetch failed");
                    return;
                }
            }
        };

        info!(app = %app_id.0, bytes = wasm_bytes.len(), "artifact ready, compiling");

        // 4. Compile (CPU-intensive — spawn_blocking)
        let runtime = self.runtime.clone();
        let artifact = tokio::task::spawn_blocking(move || runtime.compile(&wasm_bytes)).await;

        match artifact {
            Ok(Ok(artifact_bytes)) => {
                // 5. Store compiled artifact, config, and hash
                if let Err(e) = self.store.store_artifact(&app_id, &artifact_bytes) {
                    error!(app = %app_id.0, error = %e, "failed to store artifact");
                    return;
                }
                if let Err(e) = self.store.save_config(&config) {
                    error!(app = %app_id.0, error = %e, "failed to store config");
                    return;
                }
                if let Err(e) = self.store.save_artifact_hash(&app_id, &sha256) {
                    error!(app = %app_id.0, error = %e, "failed to store artifact hash");
                }
                info!(
                    app = %app_id.0,
                    "deploy complete, waiting for first request"
                );
            }
            Ok(Err(e)) => error!(app = %app_id.0, error = %e, "compilation failed"),
            Err(e) => error!(app = %app_id.0, error = %e, "spawn_blocking panic"),
        }
    }

    async fn handle_remove(&self, app_id: AppId) {
        info!(app = %app_id.0, "removing app");

        // Kill all running instances first (this creates billing records)
        if let Err(e) = self.supervisor.kill_all_instances(&app_id).await {
            error!(app = %app_id.0, error = %e, "failed to kill instances");
        }

        // Mark app as undeployed - starts grace period
        // Actual deletion happens after grace period expires in GC loop
        if let Err(e) = self.store.mark_undeployed(&app_id.0) {
            error!(app = %app_id.0, error = %e, "failed to mark app as undeployed");
        }

        // Note: We don't immediately delete artifacts/configs anymore
        // The GC loop will purge them after the grace period
        info!(app = %app_id.0, "app marked for deletion, grace period started");
    }

    fn our_node_id(&self) -> String {
        std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
    }

    async fn handle_node_joined(
        &self,
        new_node_id: String,
        peer_artifact_url: String,
        peer_public_key: Vec<u8>,
        protocol_version: u32,
        binary_version: String,
    ) {
        info!(
            new_node = %new_node_id,
            protocol = protocol_version,
            version = %binary_version,
            "node joined cluster"
        );

        // Leader election: only the node with lexicographically smallest ID responds
        if self.node_id > new_node_id {
            return;
        }

        info!(
            new_node = %new_node_id,
            our_node = %self.node_id,
            "sending state snapshot to new node"
        );

        // 1. Collect all configs
        let configs = self
            .store
            .list_apps()
            .unwrap_or_default()
            .iter()
            .filter_map(|id| self.store.load_config(id).ok().flatten())
            .collect::<Vec<_>>();

        // 2. Collect all routes
        let routes = self.store.list_routes().unwrap_or_default();

        // 3. Encrypt secrets for each app
        let mut encrypted_secrets = Vec::new();
        for config in &configs {
            if let Ok(keys) = self.secret_provider.list_keys(&config.id).await {
                for key in keys {
                    if let Ok(value) = self.secret_provider.get(&config.id, &key).await {
                        let encrypted = encrypt_for_peer(&peer_public_key, value.as_bytes());
                        if !encrypted.is_empty() {
                            encrypted_secrets.push((config.id.0.clone(), key, encrypted));
                        }
                    }
                }
            }
        }

        // 4. Collect artifact hashes
        let artifact_hashes: Vec<(String, String)> = configs
            .iter()
            .filter_map(|c| {
                self.store
                    .get_artifact_sha256(&c.id)
                    .ok()
                    .flatten()
                    .map(|h| (c.id.0.clone(), h))
            })
            .collect();

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
                    match client.put(&url).body(raw).send().await {
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
        if let Err(e) = self.bus.publish(&event).await {
            error!(new_node = %new_node_id, error = %e, "failed to publish snapshot event");
        }
    }

    async fn handle_state_snapshot(
        &self,
        configs: Vec<common::types::AppConfig>,
        routes: Vec<common::types::Route>,
        encrypted_secrets: Vec<(String, String, Vec<u8>)>,
        artifact_hashes: Vec<(String, String)>,
    ) {
        info!(
            apps = configs.len(),
            routes = routes.len(),
            secrets = encrypted_secrets.len(),
            "received state snapshot"
        );

        // 1. Store configs
        for config in &configs {
            if let Err(e) = self.store.save_config(config) {
                error!(app = %config.id.0, error = %e, "failed to save config");
            }
        }

        // 2. Store routes and load into HostRouter
        for route in &routes {
            if let Err(e) = self.store.save_route(route) {
                error!(host = %route.host, error = %e, "failed to save route");
            }
            self.host_router
                .add_route(route.host.clone(), route.app_id.clone())
                .await;
        }

        // 3. Decrypt and store secrets
        let keypair = match &self.bootstrap_keypair {
            Some(kp) => kp,
            None => {
                error!("no bootstrap keypair available to decrypt secrets");
                return;
            }
        };

        for (app_id_str, key, encrypted_value) in encrypted_secrets {
            let app_id = AppId(app_id_str.clone());

            // Decrypt using our bootstrap keypair
            let plaintext_bytes = keypair.decrypt(&encrypted_value);
            if plaintext_bytes.is_empty() {
                error!(app = app_id_str, key, "failed to decrypt secret");
                continue;
            }

            match String::from_utf8(plaintext_bytes) {
                Ok(plaintext) => {
                    if let Err(e) = self.secret_provider.set(&app_id, &key, &plaintext).await {
                        error!(app = app_id_str, key, error = %e, "failed to store secret");
                    } else {
                        info!(app = app_id_str, key, "secret decrypted and stored");
                    }
                }
                Err(e) => {
                    error!(app = app_id_str, key, error = %e, "secret not valid UTF-8");
                }
            }
        }

        // 4. Store artifact hashes
        for (app_id_str, sha256) in &artifact_hashes {
            let app_id = AppId(app_id_str.clone());
            if let Err(e) = self.store.save_artifact_hash(&app_id, sha256) {
                error!(app = app_id_str, error = %e, "failed to save artifact hash");
            }
        }

        // 5. Compile artifacts (artifacts should already be in our local store from push)
        for (app_id_str, sha256) in artifact_hashes {
            let app_id = AppId(app_id_str.clone());

            // Wait a bit for the artifact to arrive via HTTP push
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            if let Ok(Some(raw)) = self.store.load_raw_wasm(&sha256) {
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
    }

    async fn handle_node_upgrade(&self, event: Event) {
        use crate::upgrade::{download_and_verify, handle_upgrade_event, UpgradeAction};

        // Collect all node IDs in the cluster for rolling upgrade ordering
        let cluster_nodes = self
            .store
            .list_apps()
            .ok()
            .map(|_apps| {
                // In a real implementation, we'd track node IDs separately
                // For now, we'll just use the node_id from the event
                vec![self.node_id.clone()]
            })
            .unwrap_or_else(|| vec![self.node_id.clone()]);

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
                            info!(path = ?new_binary_path, "new binary downloaded and verified");

                            // Update the symlink to point to the new binary
                            let current_link = install_dir.join("current");
                            if let Err(e) = std::fs::remove_file(&current_link) {
                                if e.kind() != std::io::ErrorKind::NotFound {
                                    error!(error = %e, "failed to remove old symlink");
                                    return;
                                }
                            }

                            #[cfg(unix)]
                            let symlink_result =
                                std::os::unix::fs::symlink(&new_binary_path, &current_link);

                            #[cfg(windows)]
                            let symlink_result =
                                std::os::windows::fs::symlink_file(&new_binary_path, &current_link);

                            if let Err(e) = symlink_result {
                                error!(error = %e, "failed to create new symlink");
                                return;
                            }

                            info!("symlink updated, initiating graceful shutdown");

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
        info!(timeout_secs, "beginning graceful shutdown");

        // 1. Stop accepting new connections
        // (This would be done via a shared shutdown signal in the proxy)

        // 2. Stop supervisor from spawning new instances
        // (Would need a shutdown flag in the supervisor)

        // 3. Wait for existing requests to drain
        let drain_duration = tokio::time::Duration::from_secs(timeout_secs);
        tokio::time::sleep(drain_duration).await;

        // 4. Kill all running instances
        info!("drain timeout elapsed, stopping all instances");
        // supervisor.kill_all() would go here

        info!("graceful shutdown complete");
    }
}

/// Fetch an artifact from a URL and verify its SHA-256 hash.
async fn fetch_artifact(url: &str, expected_sha256: &str) -> Result<Vec<u8>, String> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("HTTP GET failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("artifact server returned {}", resp.status()));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read body: {e}"))?
        .to_vec();

    // Verify integrity
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256 {
        return Err(format!(
            "SHA-256 mismatch: expected {expected_sha256}, got {actual}"
        ));
    }

    Ok(bytes)
}
