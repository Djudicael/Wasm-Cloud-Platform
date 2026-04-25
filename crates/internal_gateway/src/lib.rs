use axum::{
    extract::State,
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
/// Runs on a single loopback port (e.g., 127.0.0.1:9080).
/// All internal HTTP traffic with endpoint-level policies flows through here.
pub struct InternalGateway {
    /// Registry for port→app lookups and namespace-scoped app resolution.
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

    pub async fn run(self, bind: SocketAddr) -> Result<(), std::io::Error> {
        let state = Arc::new(self);
        let app = Router::new()
            .route("/*path", any(proxy_handler))
            .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
            .with_state(state);

        let listener = TcpListener::bind(bind).await?;
        tracing::info!(%bind, "internal gateway listening");
        axum::serve(listener, app).await
    }
}

async fn proxy_handler(
    State(gw): State<Arc<InternalGateway>>,
    headers: HeaderMap,
    req: Request<axum::body::Body>,
) -> Result<Response<axum::body::Body>, StatusCode> {
    // 1. Determine source app from X-Source-App header.
    // In production, this header is injected by the WASI host (the Supervisor)
    // so the Wasm app cannot forge it.
    let source_app = headers
        .get("x-source-app")
        .and_then(|v| v.to_str().ok())
        .map(|v| common::types::AppId(v.to_string()))
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // 2. Determine target app from the Host header.
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;

    let bare_name = host.trim_end_matches(".internal");

    // 3. Same-namespace check (defense in depth).
    let target_addr = gw
        .registry
        .resolve(source_app.namespace(), bare_name)
        .await
        .ok_or(StatusCode::BAD_GATEWAY)?;

    // 4. Load endpoint rules for the target app.
    let target_app_id = common::types::AppId(format!("{}/{}", source_app.namespace(), bare_name));
    let route_config = gw.gateway_config.get_route_config(&target_app_id).await;

    // 5. Apply endpoint-level auth (if configured).
    let path = req.uri().path();
    let method = req.method().as_str();
    if let Some(ref cfg) = route_config {
        if let Some(rule) = cfg.endpoints.iter().find(|e| {
            path.starts_with(&e.path)
                && (e.methods.is_empty() || e.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
        }) {
            match &rule.auth {
                common::types::EndpointAuth::None => {}
                common::types::EndpointAuth::Inherit => {}
                common::types::EndpointAuth::Authenticated => {
                    // JWT required — for internal proxy, we expect X-User-Id or similar
                    // In practice, internal m2m usually uses API keys.
                }
                common::types::EndpointAuth::Roles { .. } => {
                    // Role check would go here.
                }
                common::types::EndpointAuth::ApiKey => {
                    let api_key = headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok());
                    if let Some(key) = api_key {
                        if !gw.gateway_config.validate_api_key(&target_app_id.0, key, path).await {
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                    } else {
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                }
            }

            // Endpoint-level rate limit
            if let Some(ref rl) = rule.rate_limit {
                // Note: internal proxy uses a simplified check.
                // Full implementation would use the same limiter as Pingora.
                let _ = rl;
            }
        }
    }

    // 6. Rate limiting (app-level, simplified).
    let app_id = format!("{}/{}", source_app.namespace(), bare_name);
    let _ = app_id;
    // In a full implementation, this would call the distributed rate limiter.

    // 7. Circuit breaker.
    if gw.circuit_breaker.is_circuit_open(&target_app_id.0) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    // 8. Forward to the real target.
    let uri = format!(
        "http://{}{}",
        target_addr,
        req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("")
    );

    // Save method and headers before consuming body
    let method = req.method().clone();
    let req_headers = req.headers().clone();

    // Collect the request body into bytes
    let body_bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read request body");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let client = reqwest::Client::new();
    let mut forward_req = client.request(method, &uri);

    // Strip internal headers before forwarding.
    for (k, v) in &req_headers {
        let name = k.as_str();
        if name != "x-source-app" && name != "host" {
            forward_req = forward_req.header(k, v);
        }
    }
    // Inject the real Host header for the target app.
    forward_req = forward_req.header("host", format!("{}:{}", bare_name, target_addr.port()));

    match forward_req.body(body_bytes).send().await {
        Ok(resp) => {
            gw.circuit_breaker.record_success(&target_app_id.0);
            let mut builder = Response::builder().status(resp.status());
            for (k, v) in resp.headers() {
                builder = builder.header(k, v);
            }
            // Read response body into bytes
            let resp_bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read response body");
                    return Err(StatusCode::BAD_GATEWAY);
                }
            };
            builder.body(axum::body::Body::from(resp_bytes)).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            tracing::warn!(error = %e, "internal proxy upstream error");
            gw.circuit_breaker.record_failure(&target_app_id.0);
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn setup_gateway() -> (Arc<InternalGateway>, common::types::AppId) {
        let registry = Arc::new(supervisor::network::NamespaceRegistry::default());
        let app_id = common::types::AppId::new_namespaced("default", "target", "v1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            registry
                .register(
                    &app_id,
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
                )
                .await;
        });

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
    fn test_internal_gateway_creation() {
        let (gw, _app_id) = setup_gateway();
        assert!(!gw.circuit_breaker.is_circuit_open("test"));
    }
}
