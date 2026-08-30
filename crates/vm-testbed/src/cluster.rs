//! Cluster fixture for multi-node microVM testing.
//!
//! This module provides [`ClusterFixture`], which orchestrates a complete
//! test cluster of microVMs: one NATS VM and N wasm-node VMs.
//!
//! ## Example
//!
//! ```rust,no_run
//! use vm_testbed::cluster::ClusterFixture;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut cluster = ClusterFixture::new("test-cluster-1").await?;
//!
//!     // Start NATS VM
//!     cluster.start_nats(256, 1).await?;
//!
//!     // Start 3 wasm-node VMs
//!     for _ in 0..3 {
//!         cluster.start_node(512, 2).await?;
//!     }
//!
//!     // Wait for all nodes to be healthy
//!     cluster.wait_for_all_healthy(Duration::from_secs(60)).await?;
//!
//!     // ... run tests ...
//!
//!     cluster.teardown().await?;
//!     Ok(())
//! }
//! ```

use crate::network::{allocate_ip, setup_network, tap_name_for_id, teardown_network};
use crate::vm::{MicroVm, VmConfig, VmError};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

/// A cluster of microVMs for testing the Wasm Cloud Platform.
///
/// Manages the lifecycle of a NATS VM and multiple wasm-node VMs,
/// including network setup, IP allocation, and health monitoring.
pub struct ClusterFixture {
    pub name: String,
    pub nats: Option<MicroVm>,
    pub nodes: HashMap<String, MicroVm>,
    pub bridge_name: String,
    pub subnet: String,
    pub gateway: String,
    pub kernel_path: PathBuf,
    pub nats_rootfs: PathBuf,
    pub node_rootfs: PathBuf,
    pub node_data_drive: Option<PathBuf>,
    pub node_otlp_endpoint: Option<String>,
    next_node_index: u8,
}

/// Error type for cluster operations.
#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("VM error: {0}")]
    Vm(#[from] VmError),
    #[error("Network error: {0}")]
    Network(String),
    #[error("NATS VM not running")]
    NatsNotRunning,
    #[error("Node {0} not found")]
    NodeNotFound(String),
    #[error("Health check failed: {0}")]
    HealthCheck(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl ClusterFixture {
    /// Create a new cluster fixture with the given name.
    ///
    /// Sets up the network bridge but does not start any VMs yet.
    /// Use [`start_nats`](Self::start_nats) and [`start_node`](Self::start_node)
    /// to populate the cluster.
    pub async fn new(name: impl Into<String>) -> Result<Self, ClusterError> {
        let name = name.into();
        let bridge_name = format!("br-{}", name.replace('_', "-"));
        let subnet = "172.20.0.0/24".to_string();
        let gateway = "172.20.0.1".to_string();

        info!(%name, %bridge_name, "Creating cluster fixture");

        // Setup network
        setup_network(&bridge_name, "172.20.0.1/24", true)
            .map_err(|e| ClusterError::Network(format!("{e}")))?;

        // Discover assets
        let kernel_path = find_kernel();
        let nats_rootfs = find_nats_rootfs();
        let node_rootfs = find_node_rootfs();
        let node_data_drive = find_node_data_drive();

        info!(%name, "Cluster fixture ready");

        Ok(ClusterFixture {
            name,
            nats: None,
            nodes: HashMap::new(),
            bridge_name,
            subnet,
            gateway,
            kernel_path,
            nats_rootfs,
            node_rootfs,
            node_data_drive,
            node_otlp_endpoint: None,
            next_node_index: 0,
        })
    }

    /// Configure the OTLP endpoint embedded in subsequently started nodes.
    pub fn set_node_otlp_endpoint(&mut self, endpoint: Option<String>) {
        self.node_otlp_endpoint = endpoint;
    }

    /// Start the NATS microVM.
    ///
    /// The NATS VM runs a minimal Linux with NATS Server + JetStream.
    /// It is allocated IP `172.20.0.10` by convention.
    pub async fn start_nats(
        &mut self,
        memory_mb: usize,
        vcpus: usize,
    ) -> Result<&MicroVm, ClusterError> {
        let needs_start = self.nats.is_none();
        if !needs_start {
            warn!("NATS VM already running");
        }

        if needs_start {
            info!("Starting NATS microVM");

            let ip = "172.20.0.10".to_string();
            let vm_id = format!("{}-nats", self.name);
            let tap = tap_name_for_id(&vm_id);

            let config = VmConfig {
                id: vm_id,
                kernel_path: self.kernel_path.clone(),
                rootfs_path: self.nats_rootfs.clone(),
                rootfs_read_only: false,
                data_drive_path: None,
                memory_mb,
                vcpus,
                ip: ip.clone(),
                gateway: self.gateway.clone(),
                bridge_name: self.bridge_name.clone(),
                tap_device: tap,
                extra_kernel_args: Vec::new(),
                mmds_data: Some(json!({
                    "nats_config": {
                        "jetstream": true,
                        "store_dir": "/data/jetstream",
                        "max_memory_store": "1GB",
                        "max_file_store": "10GB",
                    }
                })),
            };

            let vm = MicroVm::spawn(config).await?;
            self.nats = Some(vm);

            // Wait for NATS to be ready
            sleep(Duration::from_secs(3)).await;

            info!("NATS microVM started");
        }

        Ok(self
            .nats
            .as_ref()
            .expect("NATS should be available after start"))
    }

    /// Start a new wasm-node microVM.
    ///
    /// Allocates the next available IP and starts the VM.
    /// Returns the node ID assigned.
    pub async fn start_node(
        &mut self,
        memory_mb: usize,
        vcpus: usize,
    ) -> Result<String, ClusterError> {
        let index = self.next_node_index;
        self.next_node_index += 1;

        let node_id = format!("{}-node-{}", self.name, index);
        let ip = allocate_ip(&self.subnet, index + 10) // +10 to avoid NATS IP
            .map_err(|e| ClusterError::Network(format!("{e}")))?;
        let tap = tap_name_for_id(&node_id);

        info!(%node_id, %ip, "Starting wasm-node microVM");

        let nats_url = format!("nats://{}:4222", self.nats_ip()?);

        let mut extra_kernel_args = Vec::new();
        if let Some(endpoint) = &self.node_otlp_endpoint {
            extra_kernel_args.push(format!("wcp.otlp_endpoint={endpoint}"));
        }

        let config = VmConfig {
            id: node_id.clone(),
            kernel_path: self.kernel_path.clone(),
            rootfs_path: self.node_rootfs.clone(),
            rootfs_read_only: false,
            data_drive_path: self.node_data_drive.clone(),
            memory_mb,
            vcpus,
            ip: ip.clone(),
            gateway: self.gateway.clone(),
            bridge_name: self.bridge_name.clone(),
            tap_device: tap,
            extra_kernel_args,
            mmds_data: Some(json!({
                "node_config": {
                    "node_id": node_id,
                    "nats_url": nats_url,
                    "ip": ip,
                    "gateway": self.gateway,
                    "proxy_port": 8080,
                    "admin_port": 9090,
                    "artifact_port": 9091,
                    "otlp_endpoint": self.node_otlp_endpoint,
                }
            })),
        };

        let vm = MicroVm::spawn(config).await?;
        self.nodes.insert(node_id.clone(), vm);

        info!(%node_id, %ip, "wasm-node microVM started");
        Ok(node_id)
    }

    /// Get a reference to a node by ID.
    pub fn get_node(&self, node_id: &str) -> Result<&MicroVm, ClusterError> {
        self.nodes
            .get(node_id)
            .ok_or_else(|| ClusterError::NodeNotFound(node_id.to_string()))
    }

    /// Get a mutable reference to a node by ID.
    pub fn get_node_mut(&mut self, node_id: &str) -> Result<&mut MicroVm, ClusterError> {
        self.nodes
            .get_mut(node_id)
            .ok_or_else(|| ClusterError::NodeNotFound(node_id.to_string()))
    }

    /// Kill a specific node (chaos testing).
    pub async fn kill_node(&mut self, node_id: &str) -> Result<(), ClusterError> {
        info!(%node_id, "Killing node");
        let node = self.get_node_mut(node_id)?;
        node.kill()?;
        Ok(())
    }

    /// Restart a specific node with the same configuration.
    ///
    /// This simulates a node process restart (L2 failure) or full rebuild (L4).
    pub async fn restart_node(&mut self, node_id: &str) -> Result<(), ClusterError> {
        info!(%node_id, "Restarting node");

        // Remove old VM
        let old_vm = self
            .nodes
            .remove(node_id)
            .ok_or_else(|| ClusterError::NodeNotFound(node_id.to_string()))?;

        let old_config = old_vm.config.clone();
        drop(old_vm); // Ensure cleanup

        // Small delay to let the OS release resources
        sleep(Duration::from_millis(500)).await;

        // Spawn new VM with same config
        let new_vm = MicroVm::spawn(old_config).await?;
        self.nodes.insert(node_id.to_string(), new_vm);

        info!(%node_id, "Node restarted");
        Ok(())
    }

    /// Wait for all nodes to report healthy.
    pub async fn wait_for_all_healthy(&mut self, timeout: Duration) -> Result<(), ClusterError> {
        info!("Waiting for all nodes to become healthy");

        let start = std::time::Instant::now();
        for (node_id, vm) in &mut self.nodes {
            let remaining = timeout.saturating_sub(start.elapsed());
            vm.wait_for_health(remaining)
                .await
                .map_err(|e| ClusterError::HealthCheck(format!("node {node_id}: {e}")))?;
        }

        info!("All nodes healthy");
        Ok(())
    }

    /// Get the NATS URL for connecting from the host.
    pub fn nats_url(&self) -> Result<String, ClusterError> {
        let ip = self.nats_ip()?;
        Ok(format!("nats://{ip}:4222"))
    }

    fn nats_ip(&self) -> Result<String, ClusterError> {
        self.nats
            .as_ref()
            .map(|vm| vm.config.ip.clone())
            .ok_or(ClusterError::NatsNotRunning)
    }

    /// Get the list of running node IDs.
    pub fn node_ids(&self) -> Vec<String> {
        self.nodes.keys().cloned().collect()
    }

    /// Get the number of running nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Teardown the entire cluster.
    ///
    /// Kills all VMs and removes the network bridge.
    pub async fn teardown(mut self) -> Result<(), ClusterError> {
        info!(%self.name, "Tearing down cluster");

        // Kill all nodes
        for (id, mut vm) in self.nodes.drain() {
            info!(%id, "Stopping node VM");
            let _ = vm.shutdown().await;
        }

        // Kill NATS
        if let Some(mut nats) = self.nats.take() {
            info!("Stopping NATS VM");
            let _ = nats.shutdown().await;
        }

        // Remove bridge
        teardown_network(&self.bridge_name, &self.subnet)
            .map_err(|e| ClusterError::Network(format!("{e}")))?;

        info!(%self.name, "Cluster teardown complete");
        Ok(())
    }
}

// ── Asset Discovery ──────────────────────────────────────────────────

/// Find the kernel image in common locations.
fn find_kernel() -> PathBuf {
    if let Ok(path) = std::env::var("VM_KERNEL_PATH") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    let candidates = [
        "./assets/vmlinux-6.1",
        "./assets/vmlinux",
        "/opt/vm-testbed/vmlinux-6.1",
        "/opt/vm-testbed/vmlinux",
        "/var/lib/vm-testbed/vmlinux",
    ];

    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }

    panic!(
        "Kernel image not found. Set VM_KERNEL_PATH or place vmlinux in ./assets/\n\
         See docs/vm-testbed/manual-setup.md for kernel build instructions."
    );
}

/// Find the NATS rootfs image.
fn find_nats_rootfs() -> PathBuf {
    if let Ok(path) = std::env::var("VM_NATS_ROOTFS") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    let candidates = [
        "./assets/nats-rootfs.ext4",
        "/opt/vm-testbed/nats-rootfs.ext4",
        "/var/lib/vm-testbed/nats-rootfs.ext4",
    ];

    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }

    panic!(
        "NATS rootfs not found. Set VM_NATS_ROOTFS or build with scripts/vm/build-nats-rootfs.sh\n\
         See docs/vm-testbed/manual-setup.md for build instructions."
    );
}

/// Find the wasm-node rootfs image.
fn find_node_rootfs() -> PathBuf {
    if let Ok(path) = std::env::var("VM_NODE_ROOTFS") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    let candidates = [
        "./assets/wasm-node-rootfs.ext4",
        "/opt/vm-testbed/wasm-node-rootfs.ext4",
        "/var/lib/vm-testbed/wasm-node-rootfs.ext4",
    ];

    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }

    panic!(
        "Node rootfs not found. Set VM_NODE_ROOTFS or build with scripts/vm/build-node-rootfs.sh\n\
         See docs/vm-testbed/manual-setup.md for build instructions."
    );
}

/// Find the optional node data drive template.
fn find_node_data_drive() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("VM_NODE_DATA_DRIVE") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    let candidates = ["./assets/node-data.ext4", "/opt/vm-testbed/node-data.ext4"];

    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }

    None
}
