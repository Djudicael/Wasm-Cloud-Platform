use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::any,
    Router,
};
use futures::future::BoxFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use supervisor::audit::{write_audit_event, AuditEvent, AuditEventType};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

/// Maximum request body size for the internal gateway (10 MB).
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Default request timeout for forwarded requests (30 seconds).
const FORWARDING_TIMEOUT: Duration = Duration::from_secs(30);

/// Identity of a caller, resolved from the NamespaceMap.
#[derive(Debug, Clone)]
struct CallerIdentity {
    namespace: String,
    app_id: String,
    #[allow(dead_code)]
    tid: u32,
}

/// The transparent internal gateway.
///
/// Listens on a single loopback port (9080) for East-West traffic between
/// apps on the same node. Applies policies (rate limiting, circuit breaker,
/// auth) and forwards requests to the target app.
///
/// Namespace enforcement: The gateway resolves caller identity via
/// `namespace_map.resolve_identity(source_port)` — a synchronous in-process
/// lookup. Cross-namespace calls are denied by default. Identity headers
/// are stripped to prevent forgery.
///
/// Flow:
/// 1. Supervisor spawns an app in namespace "staging"
/// 2. Supervisor sets service URL to `http://target.staging.internal:9080`
/// 3. App makes a plain HTTP request — no identity headers
/// 4. eBPF detects connection, consumer binds source_port → TID
/// 5. Gateway parses Host header → target is "staging/target"
/// 6. Gateway calls resolve_identity(source_port) → {ns, app_id, tid}
/// 7. Gateway enforces namespace policy, then forwards.
pub struct InternalGateway {
    /// Registry for namespace-scoped app resolution.
    pub registry: Arc<supervisor::network::NamespaceRegistry>,

    /// Policy state shared with the external Pingora gateway.
    pub rate_limiter: Arc<proxy::rate_limiter::RateLimiter>,
    pub circuit_breaker: Arc<proxy::gateway::circuit_breaker::CircuitBreakerManager>,
    pub gateway_config: Arc<proxy::gateway::Gateway>,

    /// Shared HTTP client for forwarding requests (reused across all requests).
    pub http_client: reqwest::Client,

    /// Namespace identity map — gateway calls resolve_identity(source_port).
    pub namespace_map: Option<Arc<ebpf_monitor::NamespaceMap>>,

    /// Whether eBPF namespace enforcement is active.
    pub ebpf_active: bool,

    /// Whether to allow anonymous (unidentified) internal requests.
    pub allow_anonymous_internal: bool,

    /// Callback to trigger a cold-start when the target app has no running instances.
    pub cold_start: Option<
        Arc<dyn Fn(common::types::AppId) -> BoxFuture<'static, Option<SocketAddr>> + Send + Sync>,
    >,
}

impl InternalGateway {
    pub fn new(
        registry: Arc<supervisor::network::NamespaceRegistry>,
        rate_limiter: Arc<proxy::rate_limiter::RateLimiter>,
        circuit_breaker: Arc<proxy::gateway::circuit_breaker::CircuitBreakerManager>,
        gateway_config: Arc<proxy::gateway::Gateway>,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(FORWARDING_TIMEOUT)
            .build()
            .expect("failed to build forwarding HTTP client");

        InternalGateway {
            registry,
            rate_limiter,
            circuit_breaker,
            gateway_config,
            http_client,
            namespace_map: None,
            ebpf_active: false,
            allow_anonymous_internal: false,
            cold_start: None,
        }
    }

    /// Set the namespace map for eBPF identity resolution.
    pub fn with_namespace_map(mut self, namespace_map: Arc<ebpf_monitor::NamespaceMap>) -> Self {
        self.namespace_map = Some(namespace_map);
        self
    }

    /// Set whether eBPF namespace enforcement is active.
    pub fn with_ebpf_active(mut self, active: bool) -> Self {
        self.ebpf_active = active;
        self
    }

    /// Set whether to allow anonymous internal requests.
    pub fn with_allow_anonymous(mut self, allow: bool) -> Self {
        self.allow_anonymous_internal = allow;
        self
    }

    pub fn with_cold_start(
        mut self,
        cold_start: Arc<
            dyn Fn(common::types::AppId) -> BoxFuture<'static, Option<SocketAddr>> + Send + Sync,
        >,
    ) -> Self {
        self.cold_start = Some(cold_start);
        self
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
///
/// Handles app names that contain dots (e.g., "my.api-service.production.internal")
/// by taking the last segment before ".internal" as the namespace and everything
/// before it as the app name.
fn strip_internal_identity_headers(headers: &mut HeaderMap) {
    headers.remove("x-namespace");
    headers.remove("x-source-app");
    headers.remove("x-source-tid");
}

fn unsupported_endpoint_auth_status(auth: &common::types::EndpointAuth) -> Option<StatusCode> {
    match auth {
        common::types::EndpointAuth::Authenticated
        | common::types::EndpointAuth::Roles { .. } => Some(StatusCode::NOT_IMPLEMENTED),
        _ => None,
    }
}

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
    if parts.len() >= 2 {
        // Last part is the namespace, everything before is the app name
        // (handles app names with dots, e.g., "my.api-service.production" → app="my.api-service", ns="production")
        let namespace = parts[parts.len() - 1];
        let app_name = &hostname[..hostname.len() - namespace.len() - 1];
        Some((app_name, namespace))
    } else if parts.len() == 1 {
        // Bare app name without namespace → assume "default"
        Some((parts[0], "default"))
    } else {
        None
    }
}

async fn proxy_handler(
    State(gw): State<Arc<InternalGateway>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    req: Request<axum::body::Body>,
) -> Result<Response<axum::body::Body>, StatusCode> {
    let start_time = std::time::Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let req_method_str = method.as_str().to_string();

    // ── 0. STRIP all internal identity headers ──────────────────────────
    // Prevent header reflection attacks. The gateway resolves identity
    // from the connection table, not from headers.
    let mut sanitized_headers = req.headers().clone();
    strip_internal_identity_headers(&mut sanitized_headers);

    // ── 1. RESOLVE caller identity — ask the NamespaceMap ───────────────
    let caller_identity: Option<CallerIdentity> = if peer_addr.ip().is_loopback() {
        if let Some(ref ns_map) = gw.namespace_map {
            match ns_map.resolve_identity(peer_addr.port()) {
                Some(identity) => {
                    tracing::info!(
                        source_port = peer_addr.port(),
                        namespace = %identity.namespace,
                        app_id = %identity.app_id,
                        tid = identity.tid,
                        "[INTERNAL-GW] caller identity resolved"
                    );
                    Some(CallerIdentity {
                        namespace: identity.namespace,
                        app_id: identity.app_id,
                        tid: identity.tid,
                    })
                }
                None => {
                    // Source port not in port_to_tid map
                    if gw.ebpf_active {
                        // eBPF is active but this connection is unregistered — deny
                        tracing::warn!(
                            source_port = peer_addr.port(),
                            "[INTERNAL-GW] unregistered connection — denying"
                        );
                        return Err(StatusCode::UNAUTHORIZED);
                    } else {
                        // eBPF not active — fall back to port_to_app (TOCTOU-vulnerable)
                        tracing::debug!(
                            source_port = peer_addr.port(),
                            "[INTERNAL-GW] eBPF inactive, falling back to port_to_app"
                        );
                        gw.registry
                            .resolve_source_app(peer_addr.port())
                            .await
                            .map(|app_id| CallerIdentity {
                                namespace: app_id.namespace().to_string(),
                                app_id: app_id.0.clone(),
                                tid: 0,
                            })
                    }
                }
            }
        } else {
            None
        }
    } else {
        // Non-loopback connections are never trusted for internal identity
        tracing::warn!(
            peer_addr = %peer_addr,
            "[INTERNAL-GW] non-loopback connection rejected"
        );
        return Err(StatusCode::FORBIDDEN);
    };

    // ── 2. TARGET from the Host header ──────────────────────────────────
    let host_header = sanitized_headers
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
        caller_ns = ?caller_identity.as_ref().map(|c| &c.namespace),
        caller_app = ?caller_identity.as_ref().map(|c| &c.app_id),
        "[INTERNAL-GW] request received"
    );

    // ── 3. NAMESPACE CHECK — deny cross-namespace by default ────────────
    if let Some(ref caller) = caller_identity {
        if caller.namespace != target_namespace {
            // Cross-namespace call — check allowlist
            if !gw
                .gateway_config
                .is_cross_namespace_allowed(&caller.namespace, &target_namespace)
                .await
            {
                tracing::warn!(
                    caller_ns = %caller.namespace,
                    target_ns = %target_namespace,
                    caller_app = %caller.app_id,
                    "[INTERNAL-GW] cross-namespace call DENIED"
                );
                return Err(StatusCode::FORBIDDEN);
            }
            tracing::info!(
                caller_ns = %caller.namespace,
                target_ns = %target_namespace,
                "[INTERNAL-GW] cross-namespace call ALLOWED (allowlist)"
            );
        }
    } else {
        // No identity resolved — deny by default
        if !gw.allow_anonymous_internal {
            tracing::warn!(
                source_port = peer_addr.port(),
                "[INTERNAL-GW] denying anonymous request"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
        tracing::debug!(
            source_port = peer_addr.port(),
            "[INTERNAL-GW] allowing anonymous request"
        );
    }

    // ── 3b. RATE LIMITING per source app ────────────────────────────────
    if let Some(ref caller) = caller_identity {
        let source_ip = peer_addr.ip();
        if let Err(e) = gw.rate_limiter.check_request(&caller.app_id, source_ip) {
            tracing::warn!(
                caller_app = %caller.app_id,
                error = %e,
                "[INTERNAL-GW] rate limit exceeded"
            );
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }

    // ── 4. RESOLVE target to real loopback address ──────────────────────
    let target_app_id =
        common::types::AppId::new_namespaced(&target_namespace, target_app_name, "v1");

    let target_addr = match gw
        .registry
        .resolve(&target_namespace, target_app_name)
        .await
    {
        Some(addr) => addr,
        None => {
            // Target not running — try cold start
            if let Some(ref cold_start) = gw.cold_start {
                tracing::info!(
                    target_ns = %target_namespace,
                    target_app = %target_app_name,
                    "[INTERNAL-GW] target not found, attempting cold start"
                );
                match cold_start(target_app_id.clone()).await {
                    Some(addr) => {
                        tracing::info!(%addr, "[INTERNAL-GW] cold start succeeded");
                        addr
                    }
                    None => {
                        tracing::warn!(
                            target_ns = %target_namespace,
                            target_app = %target_app_name,
                            "[INTERNAL-GW] cold start failed"
                        );
                        return Err(StatusCode::BAD_GATEWAY);
                    }
                }
            } else {
                return Err(StatusCode::BAD_GATEWAY);
            }
        }
    };

    tracing::info!(
        target_addr = %target_addr,
        "[INTERNAL-GW] resolved target address"
    );

    // ── 5. APPLY ENDPOINT POLICIES ──────────────────────────────────────
    let route_config = gw.gateway_config.get_route_config(&target_app_id).await;

    let req_path = req.uri().path();
    if let Some(ref cfg) = route_config {
        if let Some(rule) = cfg.endpoints.iter().find(|e| {
            req_path.starts_with(&e.path)
                && (e.methods.is_empty()
                    || e.methods
                        .iter()
                        .any(|m| m.eq_ignore_ascii_case(req_method_str.as_str())))
        }) {
            if let Some(status) = unsupported_endpoint_auth_status(&rule.auth) {
                tracing::warn!(
                    target_app = %target_app_id.0,
                    path = %req_path,
                    "endpoint auth mode is configured but not implemented in the internal gateway"
                );
                return Err(status);
            }

            match &rule.auth {
                common::types::EndpointAuth::None | common::types::EndpointAuth::Inherit => {}
                common::types::EndpointAuth::ApiKey => {
                    let api_key = sanitized_headers.get("x-api-key").and_then(|v| v.to_str().ok());
                    if let Some(key) = api_key {
                        if !gw
                            .gateway_config
                            .validate_api_key(&target_app_id.0, key, req_path)
                            .await
                        {
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                    } else {
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                }
                _ => unreachable!(
                    "unsupported endpoint auth modes should have been rejected earlier"
                ),
            }

            if let Some(ref rl) = rule.rate_limit {
                let _ = rl; // TODO: per-endpoint rate limiting
            }
        }
    }

    // ── 6. CIRCUIT BREAKER ──────────────────────────────────────────────
    if gw.circuit_breaker.is_circuit_open(&target_app_id.0) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── 7. FORWARD to the real target ───────────────────────────────────
    let uri = format!(
        "http://{}{}",
        target_addr,
        req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("")
    );

    let req_headers = sanitized_headers.clone();

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_SIZE).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(error = %e, "[INTERNAL-GW] failed to read request body");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let mut forward_req = gw.http_client.request(method.clone(), &uri);

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

    let result: Result<Response<axum::body::Body>, StatusCode> =
        match forward_req.body(body_bytes).send().await {
            Ok(resp) => {
                if let Some(ref caller) = caller_identity {
                    gw.rate_limiter
                        .record_success(&caller.app_id, peer_addr.ip());
                }
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
        };

    // ── AUDIT LOGGING ───────────────────────────────────────────────────
    let latency_ms = start_time.elapsed().as_millis() as u64;
    let audit_path = std::env::var("WASM_NODE_AUDIT_LOG")
        .unwrap_or_else(|_| "/var/log/wasm-node/audit.jsonl".to_string());

    let event_type = if result.is_err() {
        if let Some(ref caller) = caller_identity {
            if caller.namespace != target_namespace {
                AuditEventType::CrossNamespaceDenied
            } else {
                AuditEventType::InternalGatewayRequest
            }
        } else {
            AuditEventType::InternalGatewayRequest
        }
    } else {
        AuditEventType::InternalGatewayRequest
    };

    let event = AuditEvent {
        timestamp: chrono::Utc::now().timestamp_millis() as u64,
        node_id: std::env::var("NODE_ID").unwrap_or_else(|_| "unknown".to_string()),
        event_type,
        actor: caller_identity
            .as_ref()
            .map(|c| c.app_id.clone())
            .unwrap_or_else(|| "anonymous".to_string()),
        app_id: format!("{}/{}", target_namespace, target_app_name),
        details: serde_json::json!({
            "caller_namespace": caller_identity.as_ref().map(|c| &c.namespace),
            "caller_app_id": caller_identity.as_ref().map(|c| &c.app_id),
            "caller_tid": caller_identity.as_ref().map(|c| c.tid),
            "target_namespace": target_namespace,
            "target_app": target_app_name,
            "path": path,
            "method": req_method_str,
            "latency_ms": latency_ms,
            "source_port": peer_addr.port(),
            "allowed": result.is_ok(),
            "status_code": result.as_ref().map(|r| r.status().as_u16()).unwrap_or(0),
        }),
    };
    write_audit_event(&audit_path, &event);

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
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

        // App names with dots
        assert_eq!(
            parse_internal_host("my.api-service.production.internal"),
            Some(("my.api-service", "production"))
        );
        assert_eq!(
            parse_internal_host("a.b.c.staging.internal:9080"),
            Some(("a.b.c", "staging"))
        );
    }

    #[tokio::test]
    async fn test_internal_gateway_creation() {
        let (gw, _app_id) = setup_gateway().await;
        assert!(!gw.circuit_breaker.is_circuit_open("test"));
    }

    #[test]
    fn test_strip_internal_identity_headers_removes_forged_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-namespace", HeaderValue::from_static("forged"));
        headers.insert("x-source-app", HeaderValue::from_static("evil-app"));
        headers.insert("x-source-tid", HeaderValue::from_static("999"));
        headers.insert("host", HeaderValue::from_static("target.default.internal"));

        strip_internal_identity_headers(&mut headers);

        assert!(headers.get("x-namespace").is_none());
        assert!(headers.get("x-source-app").is_none());
        assert!(headers.get("x-source-tid").is_none());
        assert_eq!(
            headers.get("host").and_then(|v| v.to_str().ok()),
            Some("target.default.internal")
        );
    }

    #[test]
    fn test_unsupported_endpoint_auth_status_rejects_unimplemented_modes() {
        assert_eq!(
            unsupported_endpoint_auth_status(&common::types::EndpointAuth::Authenticated),
            Some(StatusCode::NOT_IMPLEMENTED)
        );
        assert_eq!(
            unsupported_endpoint_auth_status(&common::types::EndpointAuth::Roles {
                allowed_roles: vec!["admin".to_string()],
                client_id: None,
            }),
            Some(StatusCode::NOT_IMPLEMENTED)
        );
        assert_eq!(
            unsupported_endpoint_auth_status(&common::types::EndpointAuth::ApiKey),
            None
        );
    }
}
