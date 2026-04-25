use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::any,
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

/// The transparent internal gateway.
///
/// Listens on a single loopback port (9080) for East-West traffic between
/// apps on the same node. Applies policies (rate limiting, circuit breaker,
/// auth) and forwards requests to the target app.
///
/// The gateway does not enforce namespace boundaries. The `socket_addr_check`
/// blocks cross-namespace connections to direct app ports, but the gateway
/// port (9080) is open to all namespaces. Namespace isolation currently
/// relies on service discovery: the Supervisor only injects `<APP>_SERVICE_URL`
/// env vars for same-namespace apps.
///
/// Flow:
/// 1. Supervisor spawns an app in namespace "staging"
/// 2. Supervisor sets service URL to `http://target.staging.internal:9080`
/// 3. App makes a plain HTTP request — no identity headers
/// 4. Gateway parses Host header → target is "staging/target"
/// 5. Gateway resolves target, applies policies, and forwards.
pub struct InternalGateway {
    /// Registry for namespace-scoped app resolution.
    pub registry: Arc<supervisor::network::NamespaceRegistry>,

    /// Policy state shared with the external Pingora gateway.
    pub rate_limiter: Arc<proxy::rate_limiter::RateLimiter>,
    pub circuit_breaker: Arc<proxy::gateway::circuit_breaker::CircuitBreakerManager>,
    pub gateway_config: Arc<proxy::gateway::Gateway>,
}

impl InternalGateway {
    pub fn new(
        registry: Arc<supervisor::network::NamespaceRegistry>,
        rate_limiter: Arc<proxy::rate_limiter::RateLimiter>,
        circuit_breaker: Arc<proxy::gateway::circuit_breaker::CircuitBreakerManager>,
        gateway_config: Arc<proxy::gateway::Gateway>,
    ) -> Self {
        InternalGateway {
            registry,
            rate_limiter,
            circuit_breaker,
            gateway_config,
        }
    }

    /// Start the internal gateway.
    ///
    /// Binds a single TcpListener on the loopback interface and routes
    /// incoming requests through the proxy handler.
    pub async fn run(self) -> Result<(), std::io::Error> {
        let state = Arc::new(self);

        let bind_addr = SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            common::INTERNAL_GATEWAY_PORT,
        );

        let app = Router::new()
            .route("/{*path}", any(proxy_handler))
            .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
            .with_state(state.clone());

        let listener = TcpListener::bind(bind_addr).await?;
        tracing::info!(
            %bind_addr,
            "internal gateway listening"
        );

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;

        Ok(())
    }
}

/// Parse a hostname in the format `<app>.<namespace>.internal[:port]`.
/// Returns (app_name, namespace) if the format matches.
fn parse_internal_host(host: &str) -> Option<(&str, &str)> {
    // Strip port number if present (e.g., "echo-service.default.internal:9080")
    let hostname = if host.starts_with('[') {
        if let Some(bracket_end) = host.find(']') {
            &host[..bracket_end + 1]
        } else {
            host
        }
    } else {
        host.rfind(':').map(|pos| &host[..pos]).unwrap_or(host)
    };

    // Only accept hostnames that end with ".internal"
    if !hostname.ends_with(".internal") {
        return None;
    }

    let hostname = hostname.trim_end_matches(".internal");
    let parts: Vec<&str> = hostname.split('.').collect();
    if parts.len() == 2 {
        Some((parts[0], parts[1]))
    } else if parts.len() == 1 {
        // Bare app name without namespace → assume "default"
        Some((parts[0], "default"))
    } else {
        None
    }
}

async fn proxy_handler(
    State(gw): State<Arc<InternalGateway>>,
    ConnectInfo(_peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    req: Request<axum::body::Body>,
) -> Result<Response<axum::body::Body>, StatusCode> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // ── 1. TARGET from the Host header ──────────────────────────────────
    let host_header = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let (target_app_name, target_namespace) =
        parse_internal_host(host_header).ok_or_else(|| {
            tracing::warn!(host = %host_header, "[INTERNAL-GW] invalid internal host format");
            StatusCode::BAD_REQUEST
        })?;

    tracing::info!(
        target_app = %target_app_name,
        target_namespace = %target_namespace,
        method = %method,
        path = %path,
        "[INTERNAL-GW] request received"
    );

    // ── 2. RESOLVE target to real loopback address ──────────────────────
    let target_addr = gw
        .registry
        .resolve(&target_namespace, target_app_name)
        .await
        .ok_or_else(|| {
            tracing::warn!(
                target_ns = %target_namespace,
                target_app = %target_app_name,
                "[INTERNAL-GW] target app not found in namespace registry"
            );
            StatusCode::BAD_GATEWAY
        })?;

    tracing::info!(
        target_addr = %target_addr,
        "[INTERNAL-GW] resolved target address"
    );

    // ── 3. APPLY ENDPOINT POLICIES ──────────────────────────────────────
    let target_app_id = common::types::AppId(format!("{}/{}", target_namespace, target_app_name));
    let route_config = gw.gateway_config.get_route_config(&target_app_id).await;

    let path = req.uri().path();
    let method = req.method().as_str();
    if let Some(ref cfg) = route_config {
        if let Some(rule) = cfg.endpoints.iter().find(|e| {
            path.starts_with(&e.path)
                && (e.methods.is_empty()
                    || e.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
        }) {
            match &rule.auth {
                common::types::EndpointAuth::None | common::types::EndpointAuth::Inherit => {}
                common::types::EndpointAuth::Authenticated => {
                    // JWT required — placeholder for future implementation.
                }
                common::types::EndpointAuth::Roles { .. } => {
                    // Role check placeholder.
                }
                common::types::EndpointAuth::ApiKey => {
                    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
                    if let Some(key) = api_key {
                        if !gw
                            .gateway_config
                            .validate_api_key(&target_app_id.0, key, path)
                            .await
                        {
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                    } else {
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                }
            }

            if let Some(ref rl) = rule.rate_limit {
                let _ = rl; // TODO: per-endpoint rate limiting
            }
        }
    }

    // ── 4. CIRCUIT BREAKER ──────────────────────────────────────────────
    if gw.circuit_breaker.is_circuit_open(&target_app_id.0) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── 5. FORWARD to the real target ───────────────────────────────────
    let uri = format!(
        "http://{}{}",
        target_addr,
        req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("")
    );

    let method = req.method().clone();
    let req_headers = req.headers().clone();

    let body_bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(error = %e, "[INTERNAL-GW] failed to read request body");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let client = reqwest::Client::new();
    let mut forward_req = client.request(method, &uri);

    // Strip internal-only headers before forwarding to the target app.
    for (k, v) in &req_headers {
        let name = k.as_str();
        if name != "x-source-app" && name != "host" {
            forward_req = forward_req.header(k, v);
        }
    }
    // Set the real Host header for the target app.
    let real_host = format!("{}:{}", target_app_name, target_addr.port());
    forward_req = forward_req.header("host", &real_host);

    match forward_req.body(body_bytes).send().await {
        Ok(resp) => {
            gw.circuit_breaker.record_success(&target_app_id.0);
            let mut builder = Response::builder().status(resp.status());
            for (k, v) in resp.headers() {
                builder = builder.header(k, v);
            }
            let resp_bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "[INTERNAL-GW] failed to read response body");
                    return Err(StatusCode::BAD_GATEWAY);
                }
            };
            builder
                .body(axum::body::Body::from(resp_bytes))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            tracing::warn!(error = %e, "[INTERNAL-GW] upstream error");
            gw.circuit_breaker.record_failure(&target_app_id.0);
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    async fn setup_gateway() -> (Arc<InternalGateway>, common::types::AppId) {
        let registry = Arc::new(supervisor::network::NamespaceRegistry::default());
        let app_id = common::types::AppId::new_namespaced("default", "target", "v1");

        registry
            .register(
                &app_id,
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
            )
            .await;

        let gw = Arc::new(InternalGateway::new(
            registry,
            Arc::new(proxy::rate_limiter::RateLimiter::new(
                proxy::rate_limiter::RateLimitConfig::default(),
            )),
            Arc::new(proxy::gateway::circuit_breaker::CircuitBreakerManager::new()),
            Arc::new(proxy::gateway::Gateway::new(None)),
        ));

        (gw, app_id)
    }

    #[test]
    fn test_parse_internal_host() {
        assert_eq!(
            parse_internal_host("echo-service.default.internal"),
            Some(("echo-service", "default"))
        );
        assert_eq!(
            parse_internal_host("api.production.internal"),
            Some(("api", "production"))
        );
        assert_eq!(
            parse_internal_host("bare-app.internal"),
            Some(("bare-app", "default"))
        );
        assert_eq!(parse_internal_host("invalid"), None);

        // With port number
        assert_eq!(
            parse_internal_host("echo-service.default.internal:9080"),
            Some(("echo-service", "default"))
        );
        assert_eq!(
            parse_internal_host("api.production.internal:9082"),
            Some(("api", "production"))
        );
    }

    #[tokio::test]
    async fn test_internal_gateway_creation() {
        let (gw, _app_id) = setup_gateway().await;
        assert!(!gw.circuit_breaker.is_circuit_open("test"));
    }
}
