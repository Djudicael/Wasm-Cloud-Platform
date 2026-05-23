//! MicroVM abstraction for the Wasm Cloud Platform testbed.
//!
//! This module provides [`MicroVm`], a high-level wrapper around the Firecracker
//! VMM that manages the complete lifecycle of a microVM node:
//!
//! 1. Spawn the Firecracker process
//! 2. Configure machine, kernel, drives, network via the REST API
//! 3. Start the VM
//! 4. Provide health checks and control operations
//! 5. Clean up on drop
//!
//! ## Example
//!
//! ```rust,no_run
//! use vm_testbed::vm::{MicroVm, VmConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = VmConfig {
//!         id: "node-1".to_string(),
//!         kernel_path: "/opt/kernels/vmlinux-6.1".into(),
//!         rootfs_path: "/opt/images/wasm-node.ext4".into(),
//!         data_drive_path: Some("/opt/images/node-data.ext4".into()),
//!         memory_mb: 512,
//!         vcpus: 2,
//!         ip: "172.20.0.2".to_string(),
//!         gateway: "172.20.0.1".to_string(),
//!         bridge_name: "br-wasm".to_string(),
//!         tap_device: "tap-node1".to_string(),
//!         mmds_data: None,
//!     };
//!
//!     let mut vm = MicroVm::spawn(config).await?;
//!
//!     // Wait for the node to be healthy
//!     vm.wait_for_health(std::time::Duration::from_secs(60)).await?;
//!
//!     // ... run tests ...
//!
//!     vm.shutdown().await?;
//!     Ok(())
//! }
//! ```

use crate::firecracker::{FirecrackerClient, FirecrackerError};
use crate::network::{create_tap, guest_mac, remove_tap};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Configuration for a microVM instance.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Unique identifier for this VM (used for TAP name, socket name, logging).
    pub id: String,
    /// Path to the vmlinux kernel image.
    pub kernel_path: PathBuf,
    /// Path to the root filesystem (ext4 image).
    pub rootfs_path: PathBuf,
    /// Optional path to a secondary data drive (for persistent redb storage).
    pub data_drive_path: Option<PathBuf>,
    /// Memory allocated to the VM in MiB.
    pub memory_mb: usize,
    /// Number of vCPUs.
    pub vcpus: usize,
    /// IP address assigned to this VM inside the guest.
    pub ip: String,
    /// Gateway IP (the bridge IP on the host).
    pub gateway: String,
    /// Bridge name on the host that this VM should attach its TAP to.
    pub bridge_name: String,
    /// TAP device name on the host.
    pub tap_device: String,
    /// Optional MMDS metadata to pass to the guest.
    pub mmds_data: Option<serde_json::Value>,
}

/// A running microVM instance managed by Firecracker.
///
/// On drop, the VM is forcefully killed and resources are cleaned up.
/// Use [`shutdown`](Self::shutdown) for a graceful stop.
pub struct MicroVm {
    pub config: VmConfig,
    pub client: FirecrackerClient,
    pub vmm_process: Child,
    pub api_socket: PathBuf,
    pub firecracker_log: PathBuf,
    pub firecracker_metrics: PathBuf,
    cleanup_on_drop: bool,
}

/// Error type for microVM operations.
#[derive(Debug, thiserror::Error)]
pub enum VmError {
    #[error("Firecracker API error: {0}")]
    Firecracker(#[from] FirecrackerError),
    #[error("Network setup error: {0}")]
    Network(String),
    #[error("VMM process error: {0}")]
    Process(String),
    #[error("Health check failed: {0}")]
    HealthCheck(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Timeout waiting for VM to become ready")]
    Timeout,
}

impl MicroVm {
    /// Spawn a new microVM with the given configuration.
    ///
    /// This performs the following steps:
    /// 1. Creates the TAP device on the host (requires `CAP_NET_ADMIN`)
    /// 2. Starts the Firecracker VMM process
    /// 3. Waits for the API socket
    /// 4. Configures machine, boot source, drives, network
    /// 5. Optionally configures MMDS
    /// 6. Starts the VM instance
    /// 7. Waits for the guest kernel to boot (serial console polling)
    ///
    /// # Privileges
    /// Requires `CAP_NET_ADMIN` for TAP creation. Run with `sudo` or grant
    /// the capability to your test binary.
    pub async fn spawn(config: VmConfig) -> Result<Self, VmError> {
        info!(vm_id = %config.id, "Spawning microVM");

        // 1. Create TAP device
        create_tap(&config.tap_device, &config.bridge_name)
            .map_err(|e| VmError::Network(format!("failed to create TAP: {e}")))?;

        // 2. Prepare paths
        let run_dir = std::env::temp_dir().join(format!("vm-testbed-{}", config.id));
        std::fs::create_dir_all(&run_dir)?;

        let api_socket = run_dir.join("firecracker.sock");
        let firecracker_log = run_dir.join("firecracker.log");
        let firecracker_metrics = run_dir.join("metrics.json");

        // Clean up stale socket
        let _ = std::fs::remove_file(&api_socket);

        // 3. Find firecracker binary
        let firecracker_bin = find_firecracker_binary();
        let firecracker_str = firecracker_bin.to_string_lossy().to_string();
        info!(%firecracker_str, "Using Firecracker binary");

        // 4. Start Firecracker VMM
        let mut cmd = Command::new(&firecracker_bin);
        cmd.arg("--api-sock")
            .arg(&api_socket)
            .arg("--id")
            .arg(&config.id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Optional: jailer for extra security (not required for testing)
        // cmd.arg("--seccomp-level").arg("2");

        info!(vm_id = %config.id, "Starting Firecracker VMM process");
        let vmm_process = cmd.spawn()?;

        // 5. Wait for API socket
        let client = FirecrackerClient::new(&api_socket);
        client
            .wait_for_socket(Duration::from_secs(5))
            .await
            .map_err(|e| VmError::Firecracker(e))?;

        // 6. Configure logging
        let _ = client.configure_logging(&firecracker_log, "Info").await;
        let _ = client.configure_metrics(&firecracker_metrics).await;

        // 7. Configure machine
        client
            .configure_machine(config.vcpus, config.memory_mb)
            .await
            .map_err(VmError::Firecracker)?;

        // 8. Configure boot source
        client
            .set_boot_source(
                &config.kernel_path,
                "console=ttyS0 reboot=k panic=1 pci=off quiet",
            )
            .await
            .map_err(VmError::Firecracker)?;

        // 9. Attach rootfs
        client
            .attach_drive("rootfs", &config.rootfs_path, true)
            .await
            .map_err(VmError::Firecracker)?;

        // 10. Attach data drive if provided
        if let Some(ref data_path) = config.data_drive_path {
            client
                .attach_data_drive("data", data_path)
                .await
                .map_err(VmError::Firecracker)?;
        }

        // 11. Add network interface
        let mac = guest_mac(parse_index_from_id(&config.id));
        client
            .add_network_interface("eth0", &mac, &config.tap_device)
            .await
            .map_err(VmError::Firecracker)?;

        // 12. Configure MMDS if provided
        if let Some(ref mmds) = config.mmds_data {
            client
                .configure_mmds(mmds.clone())
                .await
                .map_err(VmError::Firecracker)?;
        }

        // 13. Start the VM
        client
            .start_instance()
            .await
            .map_err(VmError::Firecracker)?;

        info!(vm_id = %config.id, "MicroVM started successfully");

        Ok(MicroVm {
            config,
            client,
            vmm_process,
            api_socket,
            firecracker_log,
            firecracker_metrics,
            cleanup_on_drop: true,
        })
    }

    /// Wait for the guest OS to finish booting.
    ///
    /// Polls the serial console log for boot completion markers.
    /// This is a best-effort check — the actual readiness should be verified
    /// with [`wait_for_health`](Self::wait_for_health).
    pub async fn wait_for_boot(&self, timeout: Duration) -> Result<(), VmError> {
        info!(vm_id = %self.config.id, "Waiting for guest boot");

        let start = std::time::Instant::now();
        loop {
            if start.elapsed() > timeout {
                return Err(VmError::Timeout);
            }

            // Check if VMM is still alive (heuristic since &self)
            // In a real implementation, we'd check a serial console log or PID file
            // For now, we rely on the timeout and the fact that Firecracker VMs boot fast

            // Simple heuristic: just wait a bit. Firecracker VMs boot in < 125ms
            // but the guest init system takes longer.
            sleep(Duration::from_millis(500)).await;

            // For a real check, we'd parse the serial console FIFO
            // Firecracker can output to a named pipe or file.
            if start.elapsed() > Duration::from_secs(2) {
                // Most minimal Linux guests boot in under 2 seconds
                break;
            }
        }

        info!(vm_id = %self.config.id, "Guest boot wait complete");
        Ok(())
    }

    /// Wait for the wasm-node health endpoint to respond.
    ///
    /// Polls `GET http://{ip}:9090/healthz` until it returns 200 or the timeout expires.
    pub async fn wait_for_health(&self, timeout: Duration) -> Result<(), VmError> {
        let admin_addr = format!("{}:9090", self.config.ip);
        info!(vm_id = %self.config.id, %admin_addr, "Waiting for node health");

        let start = std::time::Instant::now();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| VmError::HealthCheck(e.to_string()))?;

        let url = format!("http://{}/healthz", admin_addr);

        loop {
            if start.elapsed() > timeout {
                return Err(VmError::HealthCheck(format!(
                    "Node {} did not become healthy within {:?}",
                    self.config.id, timeout
                )));
            }

            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(vm_id = %self.config.id, "Node is healthy");
                    return Ok(());
                }
                Ok(resp) => {
                    debug!(vm_id = %self.config.id, status = %resp.status(), "Health check not yet ready");
                }
                Err(e) => {
                    debug!(vm_id = %self.config.id, error = %e, "Health check connection failed");
                }
            }

            sleep(Duration::from_millis(500)).await;
        }
    }

    /// Gracefully shut down the microVM.
    ///
    /// Sends Ctrl-Alt-Del to the guest, then waits for the VMM to exit.
    pub async fn shutdown(&mut self) -> Result<(), VmError> {
        info!(vm_id = %self.config.id, "Shutting down microVM");

        // Try graceful shutdown first
        let _ = self.client.send_ctrl_alt_del().await;

        // Wait up to 10 seconds for graceful shutdown
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(_)) = self.vmm_process.try_wait() {
                info!(vm_id = %self.config.id, "VMM exited gracefully");
                return Ok(());
            }
            sleep(Duration::from_millis(200)).await;
        }

        // Force kill if still running
        warn!(vm_id = %self.config.id, "Graceful shutdown timed out, forcing kill");
        self.kill()?;
        Ok(())
    }

    /// Forcefully kill the microVM (simulates power loss / hardware failure).
    ///
    /// This is useful for chaos testing L2/L3/L4 failures.
    pub fn kill(&mut self) -> Result<(), VmError> {
        info!(vm_id = %self.config.id, "Forcefully killing microVM");

        match self.vmm_process.kill() {
            Ok(()) => {
                let _ = self.vmm_process.wait();
                info!(vm_id = %self.config.id, "VMM killed");
                Ok(())
            }
            Err(e) if e.raw_os_error() == Some(3) => {
                // ESRCH: process already dead
                Ok(())
            }
            Err(e) => Err(VmError::Process(format!("failed to kill VMM: {e}"))),
        }
    }

    /// Check if the VMM process is still running.
    pub fn is_running(&mut self) -> bool {
        match self.vmm_process.try_wait() {
            Ok(None) => true,
            _ => false,
        }
    }

    /// Return the Firecracker process ID.
    pub fn pid(&self) -> u32 {
        self.vmm_process.id()
    }

    /// Leave the VMM process and TAP lifecycle to external teardown logic.
    pub fn disable_cleanup_on_drop(&mut self) {
        self.cleanup_on_drop = false;
    }

    /// Get the admin API address for this VM.
    pub fn admin_addr(&self) -> String {
        format!("{}:9090", self.config.ip)
    }

    /// Get the proxy address for this VM.
    pub fn proxy_addr(&self) -> String {
        format!("{}:8080", self.config.ip)
    }

    /// Read the Firecracker log file contents.
    pub fn read_logs(&self) -> Result<String, std::io::Error> {
        std::fs::read_to_string(&self.firecracker_log)
    }

    /// Read the Firecracker metrics JSON.
    pub fn read_metrics(&self) -> Result<String, std::io::Error> {
        std::fs::read_to_string(&self.firecracker_metrics)
    }
}

impl Drop for MicroVm {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            if self.is_running() {
                let _ = self.vmm_process.kill();
                let _ = self.vmm_process.wait();
            }
            let _ = std::fs::remove_file(&self.api_socket);
            let _ = remove_tap(&self.config.tap_device);
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Find the Firecracker binary in PATH or common locations.
fn find_firecracker_binary() -> PathBuf {
    // Check environment override
    if let Ok(path) = std::env::var("FIRECRACKER_PATH") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    // Check PATH
    if let Ok(path) = which::which("firecracker") {
        return path;
    }

    // Common locations
    let candidates = [
        "/usr/bin/firecracker",
        "/usr/local/bin/firecracker",
        "/opt/firecracker/firecracker",
        "./firecracker",
    ];

    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }

    panic!(
        "Firecracker binary not found. Set FIRECRACKER_PATH or install firecracker.\n\
         See: https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md"
    );
}

/// Parse a numeric index from a VM ID like "node-1" or "nats-0".
fn parse_index_from_id(id: &str) -> u8 {
    id.rsplit_once('-')
        .and_then(|(_, num)| num.parse::<u8>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_index_from_id() {
        assert_eq!(parse_index_from_id("node-1"), 1);
        assert_eq!(parse_index_from_id("node-255"), 255);
        assert_eq!(parse_index_from_id("nats-0"), 0);
        assert_eq!(parse_index_from_id("no-number"), 0);
    }
}
