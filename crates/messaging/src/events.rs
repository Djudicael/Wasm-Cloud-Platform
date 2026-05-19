// crates/messaging/src/events.rs
use common::types::{AppConfig, AppId};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Default protocol version for deserialization of old messages without the field.
fn default_protocol_version() -> u32 {
    common::protocol::PROTOCOL_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    // ── Deployment ─────────────────────────────────────────────────
    DeployApp {
        app_id: AppId,
        config: AppConfig,
        /// The URL where the .wasm artifact can be fetched.
        /// Format: "http://<node-ip>:<port>/artifacts/<sha256>"
        artifact_url: String,
        /// SHA-256 hex string of the raw .wasm bytes.
        expected_hash: Option<String>,
        /// Size in bytes (for logging and progress tracking).
        #[serde(default)]
        size_bytes: u64,
    },
    RemoveApp {
        app_id: AppId,
    },

    // ── Routing ────────────────────────────────────────────────────
    RouteAdd {
        route: common::types::Route,
    },
    RouteRemove {
        host: String,
    },

    // ── Instance Lifecycle ─────────────────────────────────────────
    InstanceReady {
        app_id: AppId,
        addr: SocketAddr,
        node_id: String,
    },
    InstanceDead {
        app_id: AppId,
        addr: SocketAddr,
        node_id: String,
    },

    // ── Configuration ──────────────────────────────────────────────
    SecretUpdate {
        app_id: AppId,
        key: String,
        /// Secret payload for rotation.
        ///
        /// Intended long-term format: ciphertext encrypted with the cluster key.
        /// Current development compatibility path may still send UTF-8 plaintext
        /// bytes, which the receiver normalizes through the `SecretProvider`.
        encrypted_value: Vec<u8>,
    },
    ConfigUpdate {
        app_id: AppId,
        config: AppConfig,
    },
    GatewayConfigUpdate {
        app_id: AppId,
        config: common::types::GatewayRouteConfig,
    },
    GatewayConfigRemove {
        app_id: AppId,
    },

    // ── Load Reporting ────────────────────────────────────────────
    NodeLoad {
        node_id: String,
        cpu_percent: f32,
        fuel_budget_used_percent: f32,
        active_instances: u32,
    },

    // ── Cluster Bootstrap ──────────────────────────────────────────
    NodeJoined {
        node_id: String,
        /// The node's advertised artifact base URL for peer exchange.
        /// This may differ from the local listener bind address.
        artifact_server_url: String,
        /// A one-time public key for encrypting the secret transfer.
        /// (Ephemeral X25519 key, used only for this bootstrap session.)
        public_key_bytes: Vec<u8>,
        /// Protocol version of the joining node (for compatibility checks).
        #[serde(default = "default_protocol_version")]
        protocol_version: u32,
        /// Binary version string (e.g., "0.4.0").
        #[serde(default)]
        binary_version: String,
    },
    StateSnapshot {
        /// Recipient node ID.
        for_node_id: String,
        /// All app configs (JSON).
        configs: Vec<AppConfig>,
        /// All routes.
        routes: Vec<common::types::Route>,
        /// Secrets encrypted with the joining node's one-time public key.
        /// Format: Vec<(app_id, key, encrypted_value)>
        encrypted_secrets: Vec<(String, String, Vec<u8>)>,
        /// SHA-256 of each app's .wasm (so node can fetch artifacts).
        artifact_hashes: Vec<(String, String)>, // (app_id, sha256)
    },

    // ── Platform Upgrades ──────────────────────────────────────────
    NodeUpgrade {
        /// Which node should upgrade. Use "*" for all nodes (rolling).
        target_node: String,
        /// URL to download the new binary from the artifact registry.
        binary_url: String,
        /// Expected SHA-256 hash of the new binary.
        binary_sha256: String,
        /// The new binary's protocol version. Used for compatibility checks.
        new_protocol_version: u32,
        /// The new binary version string (e.g., "0.5.0").
        new_binary_version: String,
    },
    NodeUpgradeComplete {
        node_id: String,
        new_binary_version: String,
        new_protocol_version: u32,
    },
    NodeDraining {
        node_id: String,
        /// Expected time until shutdown (seconds).
        drain_timeout_secs: u64,
    },

    // ── eBPF Monitor Events ──────────────────────────────────────────
    /// A node is under memory or I/O pressure detected by eBPF.
    /// Other nodes should stop steering traffic to it.
    NodeUnderPressure {
        node_id: String,
        /// 1 = medium, 2 = critical
        pressure_level: u32,
    },

    /// A node recovered from pressure.
    NodePressureRecovered {
        node_id: String,
    },

    /// Security incident: a Wasm instance made a privileged syscall
    /// detected by the eBPF syscall anomaly detector.
    SecurityIncident {
        node_id: String,
        /// App ID of the offending instance (if known).
        app_id: String,
        /// PID of the offending process.
        pid: u32,
        /// Syscall number that triggered the incident.
        syscall_nr: u64,
        /// Category of the suspicious syscall (e.g., "PrivilegeEscalation").
        category: String,
    },

    // ── Configuration Hot-Reload ──────────────────────────────────────────────
    /// A node changed its hot-reloadable configuration.
    /// This event is informational only — peer nodes do NOT auto-apply it.
    /// Operators who want cluster-wide consistency must apply the same change
    /// to each node individually.
    ConfigHotReload {
        node_id: String,
        /// The hot-config fields that changed (serialized HotConfigUpdate).
        changes: serde_json::Value,
    },

    // ── Health Events ──────────────────────────────────────────────
    /// Published when a node's health status changes.
    NodeHealthChanged {
        node_id: String,
        /// The new health status: "healthy", "degraded", or "unhealthy".
        status: String,
        /// Which dependency caused the change (if applicable).
        cause: Option<String>,
        /// ISO-8601 timestamp.
        timestamp: String,
        /// Number of active instances.
        active_instances: u32,
        /// Whether the node is accepting requests.
        accepting_requests: bool,
    },

    /// Published periodically with the node's current health snapshot.
    NodeHealthSnapshot {
        node_id: String,
        status: String,
        active_instances: u32,
        deployed_apps: u32,
        nats_connected: bool,
        disk_free_mb: u64,
        memory_used_mb: u64,
        timestamp: String,
    },
}

impl Event {
    /// Convert to NATS subject string.
    pub fn subject(&self) -> String {
        match self {
            Event::DeployApp { .. } => "deploy.app.new".to_string(),
            Event::RemoveApp { .. } => "deploy.app.remove".to_string(),
            Event::RouteAdd { .. } => "routes.add".to_string(),
            Event::RouteRemove { .. } => "routes.remove".to_string(),
            Event::InstanceReady {
                app_id, node_id, ..
            } => {
                format!("instance.ready.{}.{}", app_id.0, node_id)
            }
            Event::InstanceDead {
                app_id, node_id, ..
            } => {
                format!("instance.dead.{}.{}", app_id.0, node_id)
            }
            Event::SecretUpdate { app_id, .. } => {
                format!("secrets.update.{}", app_id.0)
            }
            Event::ConfigUpdate { app_id, .. } => {
                format!("config.update.{}", app_id.0)
            }
            Event::GatewayConfigUpdate { app_id, .. } => {
                format!("gateway.config.update.{}", app_id.0)
            }
            Event::GatewayConfigRemove { app_id, .. } => {
                format!("gateway.config.remove.{}", app_id.0)
            }
            Event::NodeLoad { node_id, .. } => {
                format!("node.load.{}", node_id)
            }
            Event::NodeJoined { node_id, .. } => {
                format!("cluster.node_joined.{}", node_id)
            }
            Event::StateSnapshot { for_node_id, .. } => {
                format!("cluster.snapshot.{}", for_node_id)
            }
            Event::NodeUpgrade { target_node, .. } => {
                if target_node == "*" {
                    "platform.upgrade.rolling".to_string()
                } else {
                    format!("platform.upgrade.{}", target_node)
                }
            }
            Event::NodeUpgradeComplete { node_id, .. } => {
                format!("platform.upgrade_complete.{}", node_id)
            }
            Event::NodeDraining { node_id, .. } => {
                format!("platform.draining.{}", node_id)
            }

            // ── eBPF Monitor Events ──────────────────────────────────
            Event::NodeUnderPressure { node_id, .. } => {
                format!("ebpf.pressure.{}", node_id)
            }
            Event::NodePressureRecovered { node_id, .. } => {
                format!("ebpf.pressure.recovered.{}", node_id)
            }
            Event::SecurityIncident { node_id, .. } => {
                format!("ebpf.security.incident.{}", node_id)
            }

            // ── Configuration Hot-Reload ──────────────────────────────────────
            Event::ConfigHotReload { node_id, .. } => {
                format!("config.hot_reload.{}", node_id)
            }

            // ── Health Events ──────────────────────────────────────────────
            Event::NodeHealthChanged { node_id, .. } => {
                format!("cluster.health.changed.{}", node_id)
            }
            Event::NodeHealthSnapshot { node_id, .. } => {
                format!("cluster.health.snapshot.{}", node_id)
            }
        }
    }
}
