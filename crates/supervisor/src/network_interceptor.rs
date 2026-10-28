use common::types::AppId;
use std::net::SocketAddr;
use std::sync::Arc;

/// Result of checking an outbound TCP connection.
#[derive(Debug)]
pub enum ConnectDecision {
    /// Allow the connection to the specified address.
    Allow(SocketAddr),
    /// Deny the connection (returns ECONNREFUSED to the Wasm module).
    Deny { reason: String },
}

/// Transparent network interceptor for East-West traffic.
/// Lives in the Supervisor and is wired into every instance's WasiCtx
/// via the `socket_addr_check` callback.
pub struct NetworkInterceptor {
    pub registry: Arc<super::network::NamespaceRegistry>,
    pub source_app: AppId,
    store: Option<storage::Store>,
}

impl NetworkInterceptor {
    pub fn new(registry: Arc<super::network::NamespaceRegistry>, source_app: AppId) -> Self {
        NetworkInterceptor {
            registry,
            source_app,
            store: None,
        }
    }

    pub fn with_store(mut self, store: storage::Store) -> Self {
        self.store = Some(store);
        self
    }

    /// Called for every outbound TCP connect.
    /// Returns the **rewritten** destination address, or denies the connection.
    pub async fn check_connect(
        &self,
        _source_addr: SocketAddr,
        dest_addr: SocketAddr,
    ) -> ConnectDecision {
        tracing::info!(
            source_app = %self.source_app.0,
            dest = %dest_addr,
            "[INTERCEPTOR DEBUG] check_connect called"
        );

        let target = if dest_addr.ip().is_loopback() {
            self.registry.resolve_app_by_port(dest_addr.port()).await
        } else {
            self.registry.resolve_app_by_endpoint(dest_addr).await
        };
        if let Some(target_app) = target {
            if let Some(store) = &self.store {
                match store.load_gateway_config(&target_app.0) {
                    Ok(Some(config)) if config.requires_mesh_gateway() => {
                        return ConnectDecision::Deny {
                            reason: "target requires the internal gateway".into(),
                        }
                    }
                    Err(_) => {
                        return ConnectDecision::Deny {
                            reason: "target policy unavailable".into(),
                        }
                    }
                    _ => {}
                }
            }
            tracing::info!(
                target_app = %target_app.0,
                target_ns = %target_app.namespace(),
                source_ns = %self.source_app.namespace(),
                "[INTERCEPTOR DEBUG] found target app by port"
            );

            // Cross-namespace block
            if target_app.namespace() != self.source_app.namespace() {
                tracing::warn!(
                    source_ns = %self.source_app.namespace(),
                    target_ns = %target_app.namespace(),
                    "[INTERCEPTOR DEBUG] CROSS-NAMESPACE DENY"
                );
                return ConnectDecision::Deny {
                    reason: format!(
                        "cross-namespace connection blocked: {} → {}",
                        self.source_app.namespace(),
                        target_app.namespace()
                    ),
                };
            }

            tracing::info!("[INTERCEPTOR DEBUG] same-namespace — allow");
            return ConnectDecision::Allow(dest_addr);
        }

        // 3. Unknown loopback destination — could be the internal proxy port
        // or some other local service. Allow for now; the internal proxy
        // will apply its own policies.
        tracing::info!(dest = %dest_addr, "[INTERCEPTOR DEBUG] unknown loopback — allow");
        ConnectDecision::Allow(dest_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_registry() -> (Arc<super::super::network::NamespaceRegistry>, AppId, AppId) {
        let registry = Arc::new(super::super::network::NamespaceRegistry::default());
        let app_a = AppId::new_namespaced("production", "payments", "v1");
        let app_b = AppId::new_namespaced("production", "api-b", "v1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            registry
                .register(
                    &app_a,
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10100),
                )
                .await;
            registry
                .register(
                    &app_b,
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
                )
                .await;
        });

        (registry, app_a, app_b)
    }

    #[test]
    fn test_same_namespace_allowed() {
        let (registry, app_a, _app_b) = test_registry();
        let interceptor = NetworkInterceptor::new(registry.clone(), app_a.clone());

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(async {
            interceptor
                .check_connect(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 50000),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
                )
                .await
        });

        match decision {
            ConnectDecision::Allow(addr) => assert_eq!(addr.port(), 10101),
            other => panic!("expected Allow, got {:?}", other),
        }
    }

    #[test]
    fn test_cross_namespace_denied() {
        let registry = Arc::new(super::super::network::NamespaceRegistry::default());
        let app_a = AppId::new_namespaced("ns1", "app-a", "v1");
        let app_b = AppId::new_namespaced("ns2", "app-b", "v1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            registry
                .register(
                    &app_a,
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10100),
                )
                .await;
            registry
                .register(
                    &app_b,
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
                )
                .await;
        });

        let interceptor = NetworkInterceptor::new(registry.clone(), app_a.clone());
        let decision = rt.block_on(async {
            interceptor
                .check_connect(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 50000),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
                )
                .await
        });

        match decision {
            ConnectDecision::Deny { reason } => {
                assert!(reason.contains("cross-namespace connection blocked"));
            }
            other => panic!("expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn test_external_connection_allowed() {
        let registry = Arc::new(super::super::network::NamespaceRegistry::default());
        let app_a = AppId::new_namespaced("default", "app-a", "v1");

        let interceptor = NetworkInterceptor::new(registry, app_a);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(async {
            interceptor
                .check_connect(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 50000),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443),
                )
                .await
        });

        match decision {
            ConnectDecision::Allow(addr) => assert_eq!(
                addr,
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443)
            ),
            other => panic!("expected Allow, got {:?}", other),
        }
    }
}
