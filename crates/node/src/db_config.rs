use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};

/// Database configuration for the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// pgBouncer or database proxy URL that Wasm apps will connect to.
    /// This is injected as DATABASE_URL for apps that need database access.
    /// Default: "postgres://127.0.0.1:5432"
    pub default_database_url: String,

    /// Address to check for pgBouncer health.
    /// Default: "127.0.0.1:5432"
    pub health_check_addr: String,

    /// Health check interval in seconds.
    /// Default: 30 seconds
    pub health_check_interval_secs: u64,

    /// Enable the built-in connection proxy (fallback if pgBouncer is unavailable).
    /// Default: false (prefer pgBouncer)
    pub enable_builtin_proxy: bool,

    /// Built-in proxy listen address (if enabled).
    /// Default: "127.0.0.1:5433"
    pub builtin_proxy_addr: String,

    /// Backend database address for built-in proxy.
    /// Default: "db.internal:5432"
    pub builtin_proxy_backend: String,

    /// Maximum connections for built-in proxy.
    /// Default: 20
    pub builtin_proxy_max_connections: usize,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            default_database_url: "postgres://127.0.0.1:5432".to_string(),
            health_check_addr: "127.0.0.1:5432".to_string(),
            health_check_interval_secs: 30,
            enable_builtin_proxy: false,
            builtin_proxy_addr: "127.0.0.1:5433".to_string(),
            builtin_proxy_backend: "db.internal:5432".to_string(),
            builtin_proxy_max_connections: 20,
        }
    }
}

/// Database health checker.
pub struct DatabaseHealthChecker {
    config: DatabaseConfig,
}

impl DatabaseHealthChecker {
    pub fn new(config: DatabaseConfig) -> Self {
        DatabaseHealthChecker { config }
    }

    /// Start the background health check loop.
    pub fn start(self) {
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(self.config.health_check_interval_secs));

            loop {
                interval.tick().await;

                match self.check_health().await {
                    Ok(true) => {
                        // pgBouncer is healthy - no need to log every time
                    }
                    Ok(false) => {
                        warn!(
                            addr = %self.config.health_check_addr,
                            "pgBouncer health check failed"
                        );
                    }
                    Err(e) => {
                        warn!(
                            addr = %self.config.health_check_addr,
                            error = %e,
                            "pgBouncer health check error"
                        );
                    }
                }
            }
        });
    }

    /// Check if the database proxy is healthy.
    async fn check_health(&self) -> Result<bool, std::io::Error> {
        Ok(supervisor::db_proxy::check_pgbouncer(&self.config.health_check_addr).await)
    }

    /// Check database health once and return the result.
    pub async fn check_once(&self) -> bool {
        supervisor::db_proxy::check_pgbouncer(&self.config.health_check_addr).await
    }
}

/// Database manager that coordinates pgBouncer health checks and optional built-in proxy.
pub struct DatabaseManager {
    config: DatabaseConfig,
}

impl DatabaseManager {
    pub fn new(config: DatabaseConfig) -> Self {
        DatabaseManager { config }
    }

    /// Initialize database services.
    ///
    /// This performs the following:
    /// 1. Check if pgBouncer is available
    /// 2. If not available and built-in proxy is enabled, start it
    /// 3. Start health check loop
    pub async fn initialize(self) -> Result<(), anyhow::Error> {
        // 1. Check if pgBouncer is available
        let pgbouncer_available =
            supervisor::db_proxy::check_pgbouncer(&self.config.health_check_addr).await;

        if pgbouncer_available {
            info!(
                addr = %self.config.health_check_addr,
                "pgBouncer is available"
            );
        } else {
            warn!(
                addr = %self.config.health_check_addr,
                "pgBouncer is not available"
            );

            // 2. Start built-in proxy if enabled
            if self.config.enable_builtin_proxy {
                info!(
                    listen = %self.config.builtin_proxy_addr,
                    backend = %self.config.builtin_proxy_backend,
                    max_connections = self.config.builtin_proxy_max_connections,
                    "starting built-in database connection proxy"
                );

                let proxy = supervisor::db_proxy::ConnectionProxy::new(
                    self.config.builtin_proxy_max_connections,
                    self.config.builtin_proxy_backend.clone(),
                );

                let listen_addr = self.config.builtin_proxy_addr.clone();
                tokio::spawn(async move {
                    if let Err(e) = proxy.run(&listen_addr).await {
                        warn!(error = %e, "built-in database proxy failed");
                    }
                });

                info!("built-in database proxy started");
            } else {
                warn!(
                    "pgBouncer is not available and built-in proxy is disabled. \
                     Apps requiring database access may fail to connect."
                );
            }
        }

        // 3. Start health check loop
        let health_checker = DatabaseHealthChecker::new(self.config.clone());
        health_checker.start();

        Ok(())
    }

    pub fn default_database_url(&self) -> &str {
        &self.config.default_database_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_database_config_defaults() {
        let config = DatabaseConfig::default();
        assert_eq!(config.default_database_url, "postgres://127.0.0.1:5432");
        assert_eq!(config.health_check_addr, "127.0.0.1:5432");
        assert_eq!(config.health_check_interval_secs, 30);
        assert!(!config.enable_builtin_proxy);
    }

    #[tokio::test]
    async fn test_health_checker() {
        // Start a dummy server
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
            }
        });

        let config = DatabaseConfig {
            health_check_addr: addr.to_string(),
            ..Default::default()
        };

        let checker = DatabaseHealthChecker::new(config);
        assert!(checker.check_once().await);
    }
}
