// crates/supervisor/src/instance.rs
use common::error::PlatformError;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Duration};

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
