use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

/// A simple TCP connection pool proxy.
///
/// This proxy listens on a local port and forwards connections to a backend database server.
/// It limits the number of simultaneous backend connections using a semaphore.
///
/// **Important limitations**:
/// - This does NOT understand the PostgreSQL protocol
/// - It performs raw byte forwarding (bidirectional copy)
/// - Does NOT handle PostgreSQL session state (prepared statements, SET commands, etc.)
///
/// **Recommendation**: Use pgBouncer in transaction mode for PostgreSQL.
/// This proxy is only appropriate for:
/// - Simple TCP services without session state
/// - Redis or similar stateless protocols
/// - Edge/embedded deployments where pgBouncer cannot be installed
pub struct ConnectionProxy {
    /// Maximum simultaneous connections to the real backend.
    pool_semaphore: Arc<Semaphore>,

    /// Backend database address (e.g., "db.internal:5432")
    backend_addr: String,
}

impl ConnectionProxy {
    /// Create a new connection proxy.
    ///
    /// # Arguments
    /// * `max_connections` - Maximum number of simultaneous backend connections
    /// * `backend_addr` - Backend database server address (host:port)
    pub fn new(max_connections: usize, backend_addr: String) -> Self {
        ConnectionProxy {
            pool_semaphore: Arc::new(Semaphore::new(max_connections)),
            backend_addr,
        }
    }

    /// Run the proxy server.
    ///
    /// This function listens on the specified address and proxies connections to the backend.
    /// It blocks until the server stops (which currently never happens).
    ///
    /// # Arguments
    /// * `listen_addr` - Local address to bind to (e.g., "127.0.0.1:5433")
    pub async fn run(&self, listen_addr: &str) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(listen_addr).await?;
        info!(addr = listen_addr, backend = %self.backend_addr, "DB connection proxy listening");

        loop {
            let (client, client_addr) = listener.accept().await?;
            debug!(client = %client_addr, "proxy: new client connection");

            let sem = self.pool_semaphore.clone();
            let backend = self.backend_addr.clone();

            tokio::spawn(async move {
                // Acquire a slot in the pool (blocks if pool is full)
                let _permit = match sem.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(e) => {
                        error!(error = %e, "proxy: failed to acquire semaphore permit");
                        return;
                    }
                };

                debug!(
                    client = %client_addr,
                    backend = %backend,
                    "proxy: acquired backend connection slot"
                );

                // Connect to the real backend
                match TcpStream::connect(&backend).await {
                    Ok(server) => {
                        debug!(
                            client = %client_addr,
                            backend = %backend,
                            "proxy: connected to backend"
                        );

                        // Bidirectional copy between client and server
                        if let Err(e) = Self::proxy_connection(client, server).await {
                            warn!(
                                client = %client_addr,
                                backend = %backend,
                                error = %e,
                                "proxy: connection error"
                            );
                        }

                        debug!(
                            client = %client_addr,
                            backend = %backend,
                            "proxy: connection closed"
                        );
                    }
                    Err(e) => {
                        error!(
                            backend = %backend,
                            error = %e,
                            "proxy: backend connect failed"
                        );

                        // Optionally: send an error message to the client
                        let _ = Self::send_error_to_client(client).await;
                    }
                }

                // _permit is dropped here → slot returned to pool
            });
        }
    }

    /// Proxy data bidirectionally between client and server.
    pub async fn proxy_connection(client: TcpStream, server: TcpStream) -> std::io::Result<()> {
        let (mut client_read, mut client_write) = client.into_split();
        let (mut server_read, mut server_write) = server.into_split();

        // Spawn two tasks:
        // 1. Copy from client to server
        // 2. Copy from server to client
        // Stop when either direction closes
        tokio::select! {
            result = tokio::io::copy(&mut client_read, &mut server_write) => {
                result?;
                // Client closed → shutdown server write half
                server_write.shutdown().await?;
            }
            result = tokio::io::copy(&mut server_read, &mut client_write) => {
                result?;
                // Server closed → shutdown client write half
                client_write.shutdown().await?;
            }
        }

        Ok(())
    }

    /// Send a simple error message to the client when backend connection fails.
    async fn send_error_to_client(mut client: TcpStream) -> std::io::Result<()> {
        // For PostgreSQL, we'd send a proper error packet here.
        // For this generic proxy, just close the connection.
        client.shutdown().await
    }
}

/// Check if pgBouncer is available at the given URL.
///
/// This performs a simple TCP connection check.
/// For more sophisticated checks, you could connect and send a PostgreSQL handshake.
///
/// # Arguments
/// * `url` - The pgBouncer address (e.g., "127.0.0.1:5432")
///
/// # Returns
/// * `true` if a TCP connection can be established
/// * `false` otherwise
pub async fn check_pgbouncer(url: &str) -> bool {
    match tokio::time::timeout(std::time::Duration::from_secs(2), TcpStream::connect(url)).await {
        Ok(Ok(_stream)) => {
            debug!(url = %url, "pgBouncer health check: OK");
            true
        }
        Ok(Err(e)) => {
            warn!(url = %url, error = %e, "pgBouncer health check: connection failed");
            false
        }
        Err(_) => {
            warn!(url = %url, "pgBouncer health check: timeout");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_connection_proxy_basic() {
        // Start a simple echo server as the backend
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = backend.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = socket.read(&mut buf).await.unwrap();
            socket.write_all(&buf[..n]).await.unwrap();
        });

        // Start the proxy with max 2 connections
        let proxy = ConnectionProxy::new(2, backend_addr.to_string());
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (client, _) = proxy_listener.accept().await.unwrap();
                let sem = proxy.pool_semaphore.clone();
                let backend = proxy.backend_addr.clone();
                tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await.unwrap();
                    let server = TcpStream::connect(&backend).await.unwrap();
                    ConnectionProxy::proxy_connection(client, server).await.ok();
                });
            }
        });

        // Give the server time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Connect to the proxy and send data
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(b"hello").await.unwrap();

        let mut buf = vec![0u8; 1024];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[tokio::test]
    async fn test_check_pgbouncer() {
        // Start a dummy server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            listener.accept().await.ok();
        });

        // Check should succeed
        assert!(check_pgbouncer(&addr.to_string()).await);

        // Check non-existent port should fail
        assert!(!check_pgbouncer("127.0.0.1:60000").await);
    }
}
