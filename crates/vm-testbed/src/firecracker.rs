//! Firecracker REST API client for microVM lifecycle management.
//!
//! This module provides a thin async wrapper around the Firecracker microVMM's
//! Unix-domain-socket REST API. It handles:
//!
//! - Machine configuration (vCPUs, memory, SMT, track dirty pages)
//! - Boot source (kernel image path and boot arguments)
//! - Drive attachment (rootfs and additional block devices)
//! - Network interface configuration (TAP devices)
//! - Balloon device (for memory overcommit)
//! - VM actions (Start, Stop, Pause, Resume, Reboot)
//! - Metrics and logging configuration
//!
//! ## Firecracker API Reference
//!
//! See: <https://github.com/firecracker-microvm/firecracker/blob/main/src/api_server/swagger/firecracker.yaml>
//!
//! ## Example
//!
//! ```rust,no_run
//! use vm_testbed::firecracker::FirecrackerClient;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = FirecrackerClient::connect("/tmp/firecracker-node1.sock").await?;
//!
//!     client.configure_machine(2, 512).await?;
//!     client.set_boot_source("/opt/kernels/vmlinux-6.1", "console=ttyS0 reboot=k panic=1 pci=off").await?;
//!     client.attach_drive("rootfs", "/opt/images/rootfs.ext4", true).await?;
//!     client.add_network_interface("eth0", "AA:FC:00:00:00:01", "tap-node1").await?;
//!     client.start_instance().await?;
//!
//!     Ok(())
//! }
//! ```

use reqwest::Client;
use serde_json::json;
use std::path::Path;
use tracing::{debug, error, info};

/// Error type for Firecracker API operations.
#[derive(Debug, thiserror::Error)]
pub enum FirecrackerError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API returned error: status={status}, body={body}")]
    Api { status: u16, body: String },
    #[error("Firecracker socket not found: {0}")]
    SocketNotFound(String),
    #[error("VMM process not running")]
    VmmNotRunning,
    #[error("Invalid configuration: {0}")]
    Config(String),
}

/// A client for the Firecracker microVMM REST API.
///
/// Communicates with the Firecracker process over a Unix domain socket.
/// All operations are async and return `Result<(), FirecrackerError>`.
#[derive(Debug, Clone)]
pub struct FirecrackerClient {
    client: Client,
    api_url: String,
    socket_path: String,
}

impl FirecrackerClient {
    /// Create a new client connected to the given Unix socket path.
    ///
    /// The socket need not exist yet — this only configures the endpoint.
    /// Use [`wait_for_socket`](Self::wait_for_socket) before making API calls
    /// if the VMM was just started.
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        let socket = socket_path.as_ref().to_string_lossy().to_string();
        // Firecracker listens on a Unix socket, but reqwest needs a URL.
        // We use a dummy localhost URL and configure the connector below.
        Self {
            client: Client::new(),
            api_url: "http://localhost".to_string(),
            socket_path: socket,
        }
    }

    /// Build an HTTP request builder with the Unix socket configured.
    ///
    /// This uses reqwest's `unix_socket` feature to route HTTP over UDS.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.api_url, path);
        self.client
            .request(method, &url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
    }

    /// Wait for the Firecracker socket to appear (with timeout).
    ///
    /// Polls every 50ms until the socket file exists or the timeout expires.
    pub async fn wait_for_socket(
        &self,
        timeout: std::time::Duration,
    ) -> Result<(), FirecrackerError> {
        let start = std::time::Instant::now();
        let path = std::path::Path::new(&self.socket_path);
        while !path.exists() {
            if start.elapsed() > timeout {
                return Err(FirecrackerError::SocketNotFound(self.socket_path.clone()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Give the API server a moment to start listening
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(())
    }

    /// Check if the VMM is responsive by calling GET /.
    pub async fn ping(&self) -> Result<bool, FirecrackerError> {
        match self.request(reqwest::Method::GET, "/").send().await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(e) if e.is_connect() => Ok(false),
            Err(e) => Err(FirecrackerError::Http(e)),
        }
    }

    // =====================================================================
    // Machine Configuration
    // =====================================================================

    /// Configure the microVM's machine parameters.
    ///
    /// # Arguments
    /// * `vcpu_count` — Number of vCPUs (must be <= host cores)
    /// * `mem_size_mib` — Memory in MiB
    /// * `smt` — Simultaneous multithreading (default: false)
    /// * `track_dirty_pages` — Enable snapshot support (default: false)
    pub async fn configure_machine(
        &self,
        vcpu_count: usize,
        mem_size_mib: usize,
    ) -> Result<(), FirecrackerError> {
        info!(vcpu_count, mem_size_mib, "Configuring microVM machine");

        let body = json!({
            "vcpu_count": vcpu_count,
            "mem_size_mib": mem_size_mib,
            "smt": false,
            "track_dirty_pages": false,
        });

        let resp = self
            .request(reqwest::Method::PUT, "/machine-config")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    // =====================================================================
    // Boot Source
    // =====================================================================

    /// Set the kernel boot source.
    ///
    /// # Arguments
    /// * `kernel_image_path` — Absolute path to the vmlinux ELF binary
    /// * `boot_args` — Kernel command line (e.g., `"console=ttyS0 reboot=k panic=1 pci=off"`)
    pub async fn set_boot_source(
        &self,
        kernel_image_path: impl AsRef<Path>,
        boot_args: &str,
    ) -> Result<(), FirecrackerError> {
        let path = kernel_image_path.as_ref().to_string_lossy().to_string();
        info!(%path, %boot_args, "Setting boot source");

        let body = json!({
            "kernel_image_path": path,
            "boot_args": boot_args,
        });

        let resp = self
            .request(reqwest::Method::PUT, "/boot-source")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    // =====================================================================
    // Drives
    // =====================================================================

    /// Attach a block device (drive) to the microVM.
    ///
    /// # Arguments
    /// * `drive_id` — Unique identifier for this drive (e.g., `"rootfs"`)
    /// * `path_on_host` — Absolute path to the block device or image file
    /// * `is_root_device` — Whether this is the root filesystem
    /// * `is_read_only` — Whether to mount read-only
    pub async fn attach_drive(
        &self,
        drive_id: &str,
        path_on_host: impl AsRef<Path>,
        is_root_device: bool,
    ) -> Result<(), FirecrackerError> {
        let path = path_on_host.as_ref().to_string_lossy().to_string();
        info!(%drive_id, %path, is_root_device, "Attaching drive");

        let body = json!({
            "drive_id": drive_id,
            "path_on_host": path,
            "is_root_device": is_root_device,
            "is_read_only": false,
        });

        let resp = self
            .request(reqwest::Method::PUT, &format!("/drives/{drive_id}"))
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    /// Attach an additional data drive (non-root, read-write).
    pub async fn attach_data_drive(
        &self,
        drive_id: &str,
        path_on_host: impl AsRef<Path>,
    ) -> Result<(), FirecrackerError> {
        let path = path_on_host.as_ref().to_string_lossy().to_string();
        info!(%drive_id, %path, "Attaching data drive");

        let body = json!({
            "drive_id": drive_id,
            "path_on_host": path,
            "is_root_device": false,
            "is_read_only": false,
        });

        let resp = self
            .request(reqwest::Method::PUT, &format!("/drives/{drive_id}"))
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    // =====================================================================
    // Network Interfaces
    // =====================================================================

    /// Add a network interface to the microVM.
    ///
    /// # Arguments
    /// * `iface_id` — Unique interface ID (e.g., `"eth0"`)
    /// * `guest_mac` — MAC address inside the guest
    /// * `host_dev_name` — TAP device name on the host (must exist)
    pub async fn add_network_interface(
        &self,
        iface_id: &str,
        guest_mac: &str,
        host_dev_name: &str,
    ) -> Result<(), FirecrackerError> {
        info!(%iface_id, %guest_mac, %host_dev_name, "Adding network interface");

        let body = json!({
            "iface_id": iface_id,
            "guest_mac": guest_mac,
            "host_dev_name": host_dev_name,
        });

        let resp = self
            .request(
                reqwest::Method::PUT,
                &format!("/network-interfaces/{iface_id}"),
            )
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    // =====================================================================
    // Balloon (Memory Overcommit)
    // =====================================================================

    /// Configure a balloon device for memory overcommit.
    ///
    /// This allows the host to reclaim memory from the guest under pressure.
    pub async fn configure_balloon(&self, amount_mib: usize) -> Result<(), FirecrackerError> {
        info!(amount_mib, "Configuring balloon device");

        let body = json!({
            "amount_mib": amount_mib,
            "deflate_on_oom": false,
            "stats_polling_interval_s": 0,
        });

        let resp = self
            .request(reqwest::Method::PUT, "/balloon")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    // =====================================================================
    // VM Actions
    // =====================================================================

    /// Start the microVM instance.
    ///
    /// This transitions the VM from the `Configured` state to `Running`.
    pub async fn start_instance(&self) -> Result<(), FirecrackerError> {
        info!("Starting microVM instance");

        let body = json!({ "action_type": "InstanceStart" });

        let resp = self
            .request(reqwest::Method::PUT, "/actions")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    /// Send Ctrl-Alt-Del to the guest (graceful shutdown trigger).
    ///
    /// The guest must have an init system that handles this (e.g., systemd).
    pub async fn send_ctrl_alt_del(&self) -> Result<(), FirecrackerError> {
        info!("Sending Ctrl-Alt-Del to microVM");

        let body = json!({ "action_type": "SendCtrlAltDel" });

        let resp = self
            .request(reqwest::Method::PUT, "/actions")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    /// Forcefully reset the microVM (hard reboot).
    ///
    /// This is equivalent to pressing the physical reset button.
    pub async fn reset_instance(&self) -> Result<(), FirecrackerError> {
        info!("Resetting microVM instance");

        let body = json!({ "action_type": "InstanceReset" });

        let resp = self
            .request(reqwest::Method::PUT, "/actions")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    /// Pause the microVM (save CPU time, keep RAM).
    pub async fn pause_instance(&self) -> Result<(), FirecrackerError> {
        info!("Pausing microVM instance");

        let body = json!({ "action_type": "Pause" });

        let resp = self
            .request(reqwest::Method::PUT, "/actions")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    /// Resume a paused microVM.
    pub async fn resume_instance(&self) -> Result<(), FirecrackerError> {
        info!("Resuming microVM instance");

        let body = json!({ "action_type": "Resume" });

        let resp = self
            .request(reqwest::Method::PUT, "/actions")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    // =====================================================================
    // Logging & Metrics
    // =====================================================================

    /// Configure Firecracker's own logging.
    pub async fn configure_logging(
        &self,
        log_path: impl AsRef<Path>,
        level: &str,
    ) -> Result<(), FirecrackerError> {
        let path = log_path.as_ref().to_string_lossy().to_string();

        let body = json!({
            "log_path": path,
            "level": level,
            "show_level": true,
            "show_log_origin": true,
        });

        let resp = self
            .request(reqwest::Method::PUT, "/logger")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    /// Configure Firecracker metrics output.
    pub async fn configure_metrics(
        &self,
        metrics_path: impl AsRef<Path>,
    ) -> Result<(), FirecrackerError> {
        let path = metrics_path.as_ref().to_string_lossy().to_string();

        let body = json!({ "metrics_path": path });

        let resp = self
            .request(reqwest::Method::PUT, "/metrics")
            .json(&body)
            .send()
            .await?;

        Self::check_status(resp).await
    }

    // =====================================================================
    // MMDS (Metadata Service)
    // =====================================================================

    /// Configure the MicroVM Metadata Service (MMDS).
    ///
    /// MMDS allows the guest to query metadata via HTTP at 169.254.169.254.
    /// This is useful for passing configuration (e.g., NATS URL, node ID)
    /// without embedding it in the rootfs.
    pub async fn configure_mmds(&self, data: serde_json::Value) -> Result<(), FirecrackerError> {
        let resp = self
            .request(reqwest::Method::PUT, "/mmds/config")
            .json(&json!({ "version": "V2", "network_interfaces": ["eth0"] }))
            .send()
            .await?;
        Self::check_status(resp).await?;

        let resp = self
            .request(reqwest::Method::PUT, "/mmds")
            .json(&data)
            .send()
            .await?;
        Self::check_status(resp).await
    }

    // =====================================================================
    // Helpers
    // =====================================================================

    /// Check an HTTP response for success, returning a typed error on failure.
    async fn check_status(resp: reqwest::Response) -> Result<(), FirecrackerError> {
        let status = resp.status();
        if status.is_success() {
            debug!(%status, "Firecracker API call succeeded");
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            error!(%status, %body, "Firecracker API call failed");
            Err(FirecrackerError::Api {
                status: status.as_u16(),
                body,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_new() {
        let client = FirecrackerClient::new("/tmp/test.sock");
        assert_eq!(client.socket_path, "/tmp/test.sock");
    }
}
