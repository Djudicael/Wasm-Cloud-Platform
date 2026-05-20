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

#[derive(Debug)]
pub enum ShutdownOutcome {
    Exited(ExecutionStats),
    TaskPanicked(String),
    TimedOut,
}

/// Billing info needed for recording fuel consumption on instance exit.
#[derive(Clone, Debug)]
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
    pub task: Option<JoinHandle<ExecutionStats>>,

    /// Send a signal to this handle to begin graceful shutdown.
    pub shutdown_tx: Option<oneshot::Sender<()>>,

    /// Billing info for this instance.
    pub billing_info: BillingInfo,

    /// OS Thread ID for eBPF namespace enforcement.
    /// Set from inside the spawn_blocking closure via gettid().
    /// None if TID registration failed or eBPF is not active.
    pub tid: Option<u32>,
}

impl ManagedInstance {
    /// Initiate graceful shutdown by sending HTTP request to /_platform/shutdown endpoint
    /// and the internal shutdown signal channel.
    pub async fn begin_shutdown(&mut self) {
        let id = self.id.clone();
        let addr = self.addr;

        tracing::info!(instance = %id.0, addr = %addr, "initiating graceful shutdown");

        // Try HTTP shutdown endpoint first (gives app chance to cleanup)
        if initiate_http_shutdown(addr).await.is_ok() {
            tracing::debug!(instance = %id.0, "HTTP shutdown signal sent");
        }

        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }

    /// Wait for the instance task to exit without dropping the JoinHandle on timeout.
    pub async fn await_exit(&mut self, grace_timeout: Duration) -> ShutdownOutcome {
        let id = self.id.clone();
        let Some(task) = self.task.as_mut() else {
            return ShutdownOutcome::TimedOut;
        };

        match timeout(grace_timeout, task).await {
            Ok(Ok(stats)) => {
                tracing::info!(
                    instance = %id.0,
                    fuel = stats.fuel_consumed,
                    "instance exited cleanly"
                );
                self.task.take();
                ShutdownOutcome::Exited(stats)
            }
            Ok(Err(e)) => {
                tracing::warn!(instance = %id.0, error = %e, "instance task panicked");
                self.task.take();
                ShutdownOutcome::TaskPanicked(e.to_string())
            }
            Err(_) => {
                tracing::warn!(
                    instance = %id.0,
                    "instance did not exit within {:?}; keeping it fenced until exit is confirmed",
                    grace_timeout
                );
                ShutdownOutcome::TimedOut
            }
        }
    }
}

/// Shared HTTP client for shutdown requests — avoids creating a new client per attempt.
static SHUTDOWN_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

/// Send HTTP POST to /_platform/shutdown endpoint.
/// Returns Ok if endpoint responds (even with error status).
async fn initiate_http_shutdown(addr: SocketAddr) -> Result<(), String> {
    let url = format!("http://{}/_platform/shutdown", addr);

    match timeout(Duration::from_secs(2), SHUTDOWN_CLIENT.post(&url).send()).await {
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
            return Err(PlatformError::runtime(format!(
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

#[cfg(test)]
mod tests {
    use super::*;
    use common::types::InstanceId;
    use runtime::limits::IoStats;

    fn dummy_stats() -> ExecutionStats {
        ExecutionStats {
            instance_id: InstanceId::new(),
            fuel_limit: 10,
            fuel_consumed: 5,
            ram_bytes: 1024,
            wall_clock_ms: 1,
            trap: None,
            io_stats: IoStats {
                open_fds_peak: 0,
                fs_bytes_written: 0,
                net_egress_bytes: 0,
                outbound_connections: 0,
            },
        }
    }

    #[tokio::test]
    async fn test_await_exit_timeout_keeps_join_handle_for_later_reap() {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = shutdown_rx.await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            dummy_stats()
        });

        let mut instance = ManagedInstance {
            id: InstanceId::new(),
            app_id: AppId("test-app:v1".to_string()),
            addr: "127.0.0.1:1".parse().unwrap(),
            state: InstanceState::Ready {
                addr: "127.0.0.1:1".parse().unwrap(),
            },
            spawned_at: Instant::now(),
            last_request_at: Instant::now(),
            request_count: 0,
            task: Some(task),
            shutdown_tx: Some(shutdown_tx),
            billing_info: BillingInfo {
                tenant_id: "tenant-a".to_string(),
                fuel_quota: 100,
                ram_bytes: 2048,
            },
            tid: None,
        };

        instance.begin_shutdown().await;
        let outcome = instance.await_exit(Duration::from_millis(1)).await;
        assert!(matches!(outcome, ShutdownOutcome::TimedOut));
        assert!(instance.task.is_some());
        assert!(instance.shutdown_tx.is_none());

        tokio::time::sleep(Duration::from_millis(75)).await;
        let outcome = instance.await_exit(Duration::from_secs(1)).await;
        assert!(matches!(outcome, ShutdownOutcome::Exited(_)));
        assert!(instance.task.is_none());
    }
}
