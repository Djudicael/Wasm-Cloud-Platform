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
