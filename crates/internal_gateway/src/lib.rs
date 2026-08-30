use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, Request, StatusCode, Uri},
    response::Response,
    routing::any,
    Router,
};
use futures::future::BoxFuture;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use proxy::service::DEFAULT_MAX_BODY_SIZE_BYTES;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use supervisor::audit::{write_audit_event, AuditEvent, AuditEventType};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

#[cfg(test)]
mod tests;

/// Default request timeout for forwarded requests (30 seconds).
const FORWARDING_TIMEOUT: Duration = Duration::from_secs(30);

/// The kernel emits the TCP identity event before the request is accepted, but
/// the userspace ring-buffer consumer runs asynchronously. Give it a small,
/// bounded window to publish the source-port binding before failing closed.
const CALLER_IDENTITY_WAIT_TIMEOUT: Duration = Duration::from_millis(50);
const CALLER_IDENTITY_POLL_INTERVAL: Duration = Duration::from_millis(1);

type ColdStartFn =
    dyn Fn(common::types::AppId) -> BoxFuture<'static, Option<SocketAddr>> + Send + Sync;

#[derive(Debug)]
struct LocalRouteBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,
    last_refill: Instant,
}

impl LocalRouteBucket {
    fn new(rate_per_second: u32, burst_capacity: u32) -> Self {
        Self {
            tokens: burst_capacity as f64,
            max_tokens: burst_capacity as f64,
            refill_rate: rate_per_second as f64,
            last_refill: Instant::now(),
        }
    }

    fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

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
/// `namespace_map.resolve_identity(source_port)` - a synchronous in-process
/// lookup. Cross-namespace calls are denied by default. Identity headers
/// are stripped to prevent forgery.
///
/// Flow:
/// 1. Supervisor spawns an app in namespace "staging"
/// 2. Supervisor sets service URL to `http://target.staging.internal:9080`
/// 3. App makes a plain HTTP request - no identity headers
/// 4. eBPF detects connection, consumer binds source_port to TID
/// 5. Gateway parses Host header - target is "staging/target"
/// 6. Gateway calls resolve_identity(source_port) - {ns, app_id, tid}
/// 7. Gateway enforces namespace policy, then forwards.
pub struct InternalGateway {
    /// Registry for namespace-scoped app resolution.
    pub registry: Arc<supervisor::network::NamespaceRegistry>,

    /// Policy state shared with the external Pingora gateway.
    pub rate_limiter: Arc<proxy::rate_limiter::RateLimiter>,
    pub circuit_breaker: Arc<proxy::gateway::circuit_breaker::CircuitBreakerManager>,
    pub gateway_config: Arc<proxy::gateway::Gateway>,

    /// Namespace identity map - gateway calls resolve_identity(source_port).
    pub namespace_map: Option<Arc<ebpf_monitor::NamespaceMap>>,

    /// Whether eBPF namespace enforcement is active.
    pub ebpf_active: bool,

    /// Loopback TCP port to bind for east-west traffic.
    pub bind_port: u16,

    /// Maximum accepted request body size, enforced via Content-Length when present.
    pub max_body_size_bytes: usize,

    /// Callback to trigger a cold-start when the target app has no running instances.
    pub cold_start: Option<Arc<ColdStartFn>>,

    /// Local token buckets for route- and endpoint-level rate limiting.
    route_rate_buckets: Arc<std::sync::Mutex<HashMap<String, LocalRouteBucket>>>,
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
            namespace_map: None,
            ebpf_active: false,
            bind_port: common::INTERNAL_GATEWAY_PORT,
            max_body_size_bytes: DEFAULT_MAX_BODY_SIZE_BYTES,
            cold_start: None,
            route_rate_buckets: Arc::new(std::sync::Mutex::new(HashMap::new())),
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

    /// Set the loopback port the internal gateway binds to.
    pub fn with_bind_port(mut self, bind_port: u16) -> Self {
        self.bind_port = bind_port;
        self
    }

    pub fn with_max_body_size_bytes(mut self, max_body_size_bytes: usize) -> Self {
        self.max_body_size_bytes = max_body_size_bytes;
        self
    }

    pub fn with_cold_start(mut self, cold_start: Arc<ColdStartFn>) -> Self {
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
            state.bind_port,
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

fn check_route_rate_limit(
    buckets: &std::sync::Mutex<HashMap<String, LocalRouteBucket>>,
    key: &str,
    rate_limit: &common::types::RouteRateLimit,
) -> bool {
    let mut buckets = buckets.lock().unwrap();
    let bucket = buckets.entry(key.to_string()).or_insert_with(|| {
        LocalRouteBucket::new(rate_limit.requests_per_second, rate_limit.burst_capacity)
    });
    if (bucket.refill_rate - rate_limit.requests_per_second as f64).abs() > f64::EPSILON
        || (bucket.max_tokens - rate_limit.burst_capacity as f64).abs() > f64::EPSILON
    {
        *bucket = LocalRouteBucket::new(rate_limit.requests_per_second, rate_limit.burst_capacity);
    }
    bucket.try_acquire()
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

fn endpoint_auth_policy(
    route_auth: &common::types::AuthPolicy,
    endpoint_auth: &common::types::EndpointAuth,
) -> Option<proxy::gateway::config::AuthPolicy> {
    match endpoint_auth {
        common::types::EndpointAuth::Inherit => Some(route_auth.clone()),
        common::types::EndpointAuth::None => Some(proxy::gateway::config::AuthPolicy::None),
        common::types::EndpointAuth::Authenticated => {
            Some(proxy::gateway::config::AuthPolicy::Authenticated)
        }
        common::types::EndpointAuth::Roles {
            allowed_roles,
            client_id,
        } => Some(proxy::gateway::config::AuthPolicy::Roles {
            allowed_roles: allowed_roles.clone(),
            client_id: client_id.clone(),
        }),
        common::types::EndpointAuth::ApiKey => None,
    }
}

async fn forward_request(
    endpoint: supervisor::network::RegisteredEndpoint,
    method: http::Method,
    path_and_query: String,
    headers: HeaderMap,
    body: axum::body::Body,
    target_app_name: &str,
) -> Result<Response<axum::body::Body>, StatusCode> {
    let stream = tokio::time::timeout(
        FORWARDING_TIMEOUT,
        tokio::net::TcpStream::connect(endpoint.addr),
    )
    .await
    .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
    .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let io = TokioIo::new(stream);

    let mut req_builder = Request::builder().method(method).uri(
        path_and_query
            .parse::<Uri>()
            .map_err(|_| StatusCode::BAD_REQUEST)?,
    );

    for (k, v) in &headers {
        let name = k.as_str();
        if name != "x-source-app" && name != "host" {
            req_builder = req_builder.header(k, v);
        }
    }

    let real_host = format!("{}:{}", target_app_name, endpoint.addr.port());
    req_builder = req_builder.header("host", &real_host);

    let forward_req = req_builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let response = if endpoint.h2c {
        let (mut sender, conn) = http2::Builder::new(TokioExecutor::new())
            .handshake(io)
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        tokio::spawn(async move {
            if let Err(error) = conn.await {
                tracing::warn!(error = %error, "[INTERNAL-GW] h2 upstream connection ended with error");
            }
        });
        tokio::time::timeout(FORWARDING_TIMEOUT, sender.send_request(forward_req))
            .await
            .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
            .map_err(|_| StatusCode::BAD_GATEWAY)?
    } else {
        let (mut sender, conn) = http1::Builder::new()
            .handshake(io)
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        tokio::spawn(async move {
            if let Err(error) = conn.await {
                tracing::warn!(error = %error, "[INTERNAL-GW] h1 upstream connection ended with error");
            }
        });
        tokio::time::timeout(FORWARDING_TIMEOUT, sender.send_request(forward_req))
            .await
            .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
            .map_err(|_| StatusCode::BAD_GATEWAY)?
    };

    Ok(map_hyper_response(response))
}

fn map_hyper_response(response: http::Response<Incoming>) -> Response<axum::body::Body> {
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, axum::body::Body::new(body))
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
        // (handles app names with dots, e.g., "my.api-service.production" -> app="my.api-service", ns="production")
        let namespace = parts[parts.len() - 1];
        let app_name = &hostname[..hostname.len() - namespace.len() - 1];
        Some((app_name, namespace))
    } else if parts.len() == 1 {
        // Bare app name without namespace -> assume "default"
        Some((parts[0], "default"))
    } else {
        None
    }
}

async fn resolve_caller_identity(
    namespace_map: &ebpf_monitor::NamespaceMap,
    source_port: u16,
) -> Option<ebpf_monitor::CallerIdentity> {
    let deadline = tokio::time::Instant::now() + CALLER_IDENTITY_WAIT_TIMEOUT;
    loop {
        if let Some(identity) = namespace_map.resolve_identity(source_port) {
            return Some(identity);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(CALLER_IDENTITY_POLL_INTERVAL).await;
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

    if let Some(content_length) = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if content_length > gw.max_body_size_bytes {
            tracing::warn!(
                content_length,
                max = gw.max_body_size_bytes,
                "[INTERNAL-GW] request body too large"
            );
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
    }

    let mut sanitized_headers = req.headers().clone();
    strip_internal_identity_headers(&mut sanitized_headers);

    let caller_identity: CallerIdentity = if peer_addr.ip().is_loopback() {
        if !gw.ebpf_active {
            tracing::warn!(
                source_port = peer_addr.port(),
                "[INTERNAL-GW] eBPF identity enforcement inactive - denying request"
            );
            return Err(StatusCode::UNAUTHORIZED);
        }

        let Some(ref ns_map) = gw.namespace_map else {
            tracing::warn!(
                source_port = peer_addr.port(),
                "[INTERNAL-GW] namespace map unavailable while eBPF enforcement is required"
            );
            return Err(StatusCode::UNAUTHORIZED);
        };

        match resolve_caller_identity(ns_map, peer_addr.port()).await {
            Some(identity) => {
                tracing::info!(
                    source_port = peer_addr.port(),
                    namespace = %identity.namespace,
                    app_id = %identity.app_id,
                    tid = identity.tid,
                    "[INTERNAL-GW] caller identity resolved"
                );
                CallerIdentity {
                    namespace: identity.namespace,
                    app_id: identity.app_id,
                    tid: identity.tid,
                }
            }
            None => {
                tracing::warn!(
                    source_port = peer_addr.port(),
                    "[INTERNAL-GW] unresolved caller identity - denying request"
                );
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
    } else {
        tracing::warn!(
            peer_addr = %peer_addr,
            "[INTERNAL-GW] non-loopback connection rejected"
        );
        return Err(StatusCode::FORBIDDEN);
    };

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
        caller_ns = %caller_identity.namespace,
        caller_app = %caller_identity.app_id,
        "[INTERNAL-GW] request received"
    );

    if caller_identity.namespace != target_namespace {
        if !gw
            .gateway_config
            .is_cross_namespace_allowed(&caller_identity.namespace, target_namespace)
            .await
        {
            tracing::warn!(
                caller_ns = %caller_identity.namespace,
                target_ns = %target_namespace,
                caller_app = %caller_identity.app_id,
                "[INTERNAL-GW] cross-namespace call DENIED"
            );
            return Err(StatusCode::FORBIDDEN);
        }
        tracing::info!(
            caller_ns = %caller_identity.namespace,
            target_ns = %target_namespace,
            "[INTERNAL-GW] cross-namespace call ALLOWED (allowlist)"
        );
    }

    let source_ip = peer_addr.ip();
    if let Err(e) = gw
        .rate_limiter
        .check_request(&caller_identity.app_id, source_ip)
    {
        tracing::warn!(
            caller_app = %caller_identity.app_id,
            error = %e,
            "[INTERNAL-GW] rate limit exceeded"
        );
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    let target_app_id =
        common::types::AppId::new_namespaced(target_namespace, target_app_name, "v1");

    let target_endpoint = match gw
        .registry
        .resolve_endpoint(target_namespace, target_app_name)
        .await
    {
        Some(endpoint) => endpoint,
        None => {
            if let Some(ref cold_start) = gw.cold_start {
                tracing::info!(
                    target_ns = %target_namespace,
                    target_app = %target_app_name,
                    "[INTERNAL-GW] target not found, attempting cold start"
                );
                match cold_start(target_app_id.clone()).await {
                    Some(addr) => {
                        tracing::info!(%addr, "[INTERNAL-GW] cold start succeeded");
                        gw.registry
                            .resolve_endpoint(target_namespace, target_app_name)
                            .await
                            .unwrap_or(supervisor::network::RegisteredEndpoint { addr, h2c: false })
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
        target_addr = %target_endpoint.addr,
        h2c = target_endpoint.h2c,
        "[INTERNAL-GW] resolved target address"
    );

    let route_config = gw.gateway_config.get_route_config(&target_app_id).await;
    let route_auth = route_config
        .as_ref()
        .map(|cfg| cfg.auth.clone())
        .unwrap_or(proxy::gateway::config::AuthPolicy::None);

    let req_path = req.uri().path();
    let endpoint_rule = route_config.as_ref().and_then(|cfg| {
        cfg.endpoints.iter().find(|e| {
            req_path.starts_with(&e.path)
                && (e.methods.is_empty()
                    || e.methods
                        .iter()
                        .any(|m| m.eq_ignore_ascii_case(req_method_str.as_str())))
        })
    });

    let mut user_identity: Option<proxy::gateway::oidc::UserIdentity> = None;

    if let Some(rule) = endpoint_rule {
        match &rule.auth {
            common::types::EndpointAuth::ApiKey => {
                let api_key = sanitized_headers
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok());
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
            _ => {
                if let Some(policy) = endpoint_auth_policy(&route_auth, &rule.auth) {
                    if policy != proxy::gateway::config::AuthPolicy::None {
                        let identity = gw
                            .gateway_config
                            .authenticate_header_map_with_policy(&sanitized_headers, &policy)
                            .await
                            .map_err(|_| StatusCode::UNAUTHORIZED)?;

                        let roles_ok = proxy::gateway::authz::authorize(&identity, &policy);
                        let scopes_ok = proxy::gateway::authz::authorize_scopes(
                            &identity,
                            &rule.required_scopes,
                        );
                        if !roles_ok || !scopes_ok {
                            return Err(StatusCode::FORBIDDEN);
                        }
                        user_identity = Some(identity);
                    }
                }
            }
        }
    } else if route_auth != proxy::gateway::config::AuthPolicy::None {
        let identity = gw
            .gateway_config
            .authenticate_header_map_with_policy(&sanitized_headers, &route_auth)
            .await
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        if !proxy::gateway::authz::authorize(&identity, &route_auth) {
            return Err(StatusCode::FORBIDDEN);
        }
        user_identity = Some(identity);
    }
    let effective_rate_limit = endpoint_rule
        .and_then(|rule| {
            rule.rate_limit
                .as_ref()
                .map(|rl| (Some(rule.path.as_str()), rl))
        })
        .or_else(|| {
            route_config
                .as_ref()
                .and_then(|cfg| cfg.rate_limit.as_ref().map(|rl| (None, rl)))
        });

    if let Some((endpoint_path, rl)) = effective_rate_limit {
        let rate_limit_key = match endpoint_path {
            Some(endpoint_path) => {
                format!("{}#{}#{}", target_app_id.0, req_method_str, endpoint_path)
            }
            None => format!("{}#route", target_app_id.0),
        };

        if !check_route_rate_limit(&gw.route_rate_buckets, &rate_limit_key, rl) {
            tracing::warn!(
                app_id = %target_app_id.0,
                key = %rate_limit_key,
                requests_per_second = rl.requests_per_second,
                burst_capacity = rl.burst_capacity,
                "[INTERNAL-GW] route rate limit exceeded"
            );
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }

        if rl.distributed {
            let limiters = gw.gateway_config.distributed_limiters.read().await;
            if let Some(limiter) = limiters.get(&target_app_id.0) {
                if !limiter.check_request().await {
                    tracing::warn!(
                        app_id = %target_app_id.0,
                        requests_per_second = rl.requests_per_second,
                        "[INTERNAL-GW] distributed route rate limit exceeded"
                    );
                    return Err(StatusCode::TOO_MANY_REQUESTS);
                }
            }
        }
    }

    if gw.circuit_breaker.is_circuit_open(&target_app_id.0) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let mut forwarded_headers = sanitized_headers.clone();
    if let Some(identity) = user_identity.as_ref() {
        if let Ok(value) = http::HeaderValue::from_str(&identity.sub) {
            forwarded_headers.insert("X-User-Id", value);
        }
        if let Some(email) = identity.email.as_ref() {
            if let Ok(value) = http::HeaderValue::from_str(email) {
                forwarded_headers.insert("X-User-Email", value);
            }
        }
        if !identity.roles.is_empty() {
            if let Ok(value) = http::HeaderValue::from_str(&identity.roles.join(",")) {
                forwarded_headers.insert("X-User-Roles", value);
            }
        }
    }

    let result: Result<Response<axum::body::Body>, StatusCode> = match forward_request(
        target_endpoint,
        method.clone(),
        path_and_query,
        forwarded_headers,
        body,
        target_app_name,
    )
    .await
    {
        Ok(resp) => {
            gw.rate_limiter
                .record_success(&caller_identity.app_id, peer_addr.ip());
            gw.circuit_breaker.record_success(&target_app_id.0);
            Ok(resp)
        }
        Err(e) => {
            tracing::warn!(status = %e, "[INTERNAL-GW] upstream error");
            gw.circuit_breaker.record_failure(&target_app_id.0);
            Err(e)
        }
    };

    let latency_ms = start_time.elapsed().as_millis() as u64;
    let audit_path = std::env::var("WASM_NODE_AUDIT_LOG")
        .unwrap_or_else(|_| "/var/log/wasm-node/audit.jsonl".to_string());

    let event_type = if result.is_err() {
        if caller_identity.namespace != target_namespace {
            AuditEventType::CrossNamespaceDenied
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
        actor: caller_identity.app_id.clone(),
        app_id: format!("{}/{}", target_namespace, target_app_name),
        details: serde_json::json!({
            "caller_namespace": &caller_identity.namespace,
            "caller_app_id": &caller_identity.app_id,
            "caller_tid": caller_identity.tid,
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
