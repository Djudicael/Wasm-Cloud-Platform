// crates/supervisor/src/instance.rs
use common::error::PlatformError;
use common::types::{AppId, InstanceId, InstanceState};
use runtime::executor::ExecutionStats;
use std::net::SocketAddr;
use std::time::Instant;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Duration};

/// Billing info needed for recording fuel consumption on instance exit.
#[derive(Clone)]
pub struct BillingInfo {
    pub tenant_id: String,
    pub fuel_quota: u64,
    pub ram_bytes: u64,
}

/// A live Wasm instance managed by the Supervisor.
pub struct ManagedInstance {
    pub id: InstanceId,
    pub app_id: AppId,
    pub addr: SocketAddr,
    pub state: InstanceState,
    pub spawned_at: Instant,
    pub last_request_at: Instant,
    pub request_count: u64,

    /// Handle to the Tokio task running the Wasm module.
    pub task: JoinHandle<ExecutionStats>,

    /// Send a signal to this handle to begin graceful shutdown.
    pub shutdown_tx: oneshot::Sender<()>,

    /// Billing info for this instance.
    pub billing_info: BillingInfo,
}

impl ManagedInstance {
    /// Initiate graceful shutdown by sending HTTP request to /_platform/shutdown endpoint.
    /// Falls back to immediate shutdown if HTTP fails.
    pub async fn initiate_shutdown(self, grace_timeout: Duration) -> Option<ExecutionStats> {
        let id = self.id.clone();
        let addr = self.addr;

        tracing::info!(instance = %id.0, addr = %addr, "initiating graceful shutdown");

        // Try HTTP shutdown endpoint first (gives app chance to cleanup)
        if initiate_http_shutdown(addr).await.is_ok() {
            tracing::debug!(instance = %id.0, "HTTP shutdown signal sent");
        }

        // Send shutdown signal via channel (consumes shutdown_tx)
        let _ = self.shutdown_tx.send(());

        // Wait for task to complete with timeout
        match timeout(grace_timeout, self.task).await {
            Ok(Ok(stats)) => {
                tracing::info!(
                    instance = %id.0,
                    fuel = stats.fuel_consumed,
                    "instance exited cleanly"
                );
                Some(stats)
            }
            Ok(Err(e)) => {
                tracing::warn!(instance = %id.0, error = %e, "instance task panicked");
                None
            }
            Err(_) => {
                tracing::warn!(
                    instance = %id.0,
                    "instance did not exit within {:?} — hard abort",
                    grace_timeout
                );
                // Task is dropped here = hard abort
                None
            }
        }
    }
}

/// Send HTTP POST to /_platform/shutdown endpoint.
/// Returns Ok if endpoint responds (even with error status).
async fn initiate_http_shutdown(addr: SocketAddr) -> Result<(), String> {
    let url = format!("http://{}/_platform/shutdown", addr);
    let client = reqwest::Client::new();

    match timeout(Duration::from_secs(2), client.post(&url).send()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("HTTP request failed: {}", e)),
        Err(_) => Err("HTTP shutdown timeout".to_string()),
    }
}

/// Wait until the TCP port is accepting connections.
/// Polls every 5ms, gives up after `max_wait`.
pub async fn wait_for_ready(addr: SocketAddr, max_wait: Duration) -> Result<(), PlatformError> {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(PlatformError::Runtime(format!(
                "instance at {addr} did not become ready in time"
            )));
        }
        match timeout(Duration::from_millis(5), TcpStream::connect(addr)).await {
            Ok(Ok(_)) => {
                tracing::info!(%addr, "instance is ready");
                return Ok(());
            }
            _ => sleep(Duration::from_millis(5)).await,
        }
    }
}
