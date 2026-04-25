use super::{
    backpressure::BackpressureSignal, gateway::Gateway, metrics::RateLimitMetrics,
    node_table::NodeLoadTable, rate_limiter::RateLimiter, router::HostRouter,
    upstream::UpstreamRegistry,
};
use async_trait::async_trait;
use common::types::AppId;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result as PingoraResult;
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;

/// Context passed through the Pingora request pipeline.
pub struct RequestCtx {
    pub app_id: Option<AppId>,
    pub upstream_addr: Option<std::net::SocketAddr>,
    pub start: std::time::Instant,
    /// Whether to strip the matched path prefix before forwarding.
    pub strip_prefix: bool,
    /// The path prefix that was matched by the router.
    pub matched_prefix: Option<String>,
    /// Trace ID extracted from the incoming `traceparent` header, or generated.
    pub trace_id: Option<String>,

    // ── New gateway fields ──────────────────────────────────────
    /// Gateway configuration for the matched route.
    pub route_config: Option<crate::gateway::config::GatewayRouteConfig>,

    /// Authenticated user identity (set by auth middleware).
    pub user_identity: Option<crate::gateway::oidc::UserIdentity>,
}

/// The main Pingora proxy service — now with gateway capabilities.
pub struct WasmProxy {
    pub router: Arc<HostRouter>,
    pub upstream: Arc<UpstreamRegistry>,
    pub rate_limiter: Arc<RateLimiter>,
    pub backpressure: BackpressureSignal,
    pub node_table: Arc<NodeLoadTable>,
    pub metrics: Option<Arc<RateLimitMetrics>>,

    // ── New gateway field ───────────────────────────────────────
    /// The API gateway (auth, CORS, circuit breaker, transforms).
    pub gateway: Arc<Gateway>,

    /// Callback to trigger a cold-start when no instances are running.
    /// Returns the address of the newly spawned instance.
    pub cold_start: Arc<
        dyn Fn(AppId) -> futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
            + Send
            + Sync,
    >,
}

impl WasmProxy {
    async fn select_upstream(&self, app_id: &AppId) -> Option<std::net::SocketAddr> {
        // 1. Try local instances first (fastest path)
        if let Some(addr) = self.upstream.next(app_id).await {
            return Some(addr);
        }

        // 2. Check if local node is overloaded
        if self.node_is_overloaded().await {
            // 3. Find a remote node with capacity
            if let Some(node) = self.node_table.least_loaded_node().await {
                // Return the remote supervisor's address for this app
                // Pingora will proxy the request to the remote node's Pingora
                return Some(node.supervisor_addr);
            }
        }

        // 4. Cold start on local node (last resort)
        tracing::info!(app_id = %app_id.0, "cold start on local node");
        (self.cold_start)(app_id.clone()).await
    }

    async fn node_is_overloaded(&self) -> bool {
        // Check local fuel consumption (simplified: check CPU via sysinfo)
        false // placeholder
    }
}

#[async_trait]
impl ProxyHttp for WasmProxy {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> Self::CTX {
        RequestCtx {
            app_id: None,
            upstream_addr: None,
            start: std::time::Instant::now(),
            strip_prefix: false,
            matched_prefix: None,
            trace_id: None,
            route_config: None,
            user_identity: None,
        }
    }

    /// Step 1: Resolve the app from the Host header.
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        // Health check bypass
        if host.is_empty() || session.req_header().uri.path() == "/_platform/health" {
            return Ok(false); // Continue without routing
        }

        // Extract or generate trace ID for distributed tracing propagation.
        let trace_id = session
            .req_header()
            .headers
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
            .and_then(|tp| {
                let parts: Vec<&str> = tp.split('-').collect();
                parts.get(1).map(|id| id.to_string())
            })
            .or_else(|| {
                let bytes: [u8; 16] = rand::random();
                Some(hex::encode(bytes))
            });
        ctx.trace_id = trace_id.clone();

        let path = session.req_header().uri.path().to_string();
        let resolved = self.router.resolve(&host, &path).await;

        let route_config = match &resolved {
            Some(r) => self.gateway.get_route_config(&r.app_id).await,
            None => None,
        };

        if let Some(r) = &resolved {
            ctx.app_id = Some(r.app_id.clone());
            ctx.strip_prefix = r.strip_prefix;
            ctx.matched_prefix = Some(r.matched_prefix.clone());
        }
        ctx.route_config = route_config.clone();

        if ctx.app_id.is_none() {
            tracing::warn!(host, path, "no route found for host+path");
            session.respond_error(502).await?;
            return Ok(true);
        }

        // 2. CORS preflight (new)
        if let Some(ref cfg) = ctx.route_config {
            if cfg.cors.is_some() && session.req_header().method == "OPTIONS" {
                self.gateway.metrics.cors_preflight_total.inc();
                return crate::gateway::cors::handle_cors_preflight(session, cfg).await;
            }
        }

        // 2.5. Find matching endpoint rule (if any)
        let method = session.req_header().method.as_str();
        let endpoint_rule = ctx.route_config.as_ref()
            .and_then(|cfg| cfg.endpoints.iter()
                .find(|e| {
                    path.starts_with(&e.path) &&
                    (e.methods.is_empty() || e.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
                })
            );

        // 3. Authentication (new) — endpoint-level override or route default
        let effective_auth = match endpoint_rule {
            Some(rule) => match &rule.auth {
                common::types::EndpointAuth::Inherit => {
                    ctx.route_config.as_ref().map(|c| c.auth.clone()).unwrap_or(crate::gateway::config::AuthPolicy::None)
                }
                common::types::EndpointAuth::None => crate::gateway::config::AuthPolicy::None,
                common::types::EndpointAuth::Authenticated => crate::gateway::config::AuthPolicy::Authenticated,
                common::types::EndpointAuth::Roles { allowed_roles, client_id } => {
                    crate::gateway::config::AuthPolicy::Roles {
                        allowed_roles: allowed_roles.clone(),
                        client_id: client_id.clone(),
                    }
                }
                common::types::EndpointAuth::ApiKey => {
                    // API key auth: check X-Api-Key header
                    let api_key = session.req_header().headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok());
                    if let Some(key) = api_key {
                        let app_id = ctx.app_id.as_ref().map(|a| a.0.as_str()).unwrap_or("");
                        let path = session.req_header().uri.path();
                        if !self.gateway.validate_api_key(app_id, key, path).await {
                            return crate::gateway::errors::send_gateway_error(
                                session,
                                401,
                                "unauthorized",
                                "invalid X-Api-Key",
                            ).await;
                        }
                        crate::gateway::config::AuthPolicy::None
                    } else {
                        return crate::gateway::errors::send_gateway_error(
                            session,
                            401,
                            "unauthorized",
                            "missing X-Api-Key header",
                        ).await;
                    }
                }
            },
            None => ctx.route_config.as_ref().map(|c| c.auth.clone()).unwrap_or(crate::gateway::config::AuthPolicy::None),
        };

        if effective_auth != crate::gateway::config::AuthPolicy::None {
            match self.gateway.authenticate_with_policy(session, &effective_auth).await {
                Ok(identity) => {
                    self.gateway.metrics.auth_success_total.inc();
                    ctx.user_identity = Some(identity);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "authentication failed");
                    self.gateway.metrics.auth_failure_total.inc();
                    return crate::gateway::errors::send_gateway_error(
                        session,
                        401,
                        "unauthorized",
                        "missing or invalid JWT token",
                    )
                    .await;
                }
            }
        }

        // 4. Authorization (new) — endpoint-level role check
        if let Some(ref identity) = ctx.user_identity {
            let authorized = match endpoint_rule {
                Some(rule) => match &rule.auth {
                    common::types::EndpointAuth::Roles { allowed_roles, client_id } => {
                        crate::gateway::authz::authorize_roles(identity, allowed_roles, client_id.as_deref())
                    }
                    _ => true, // other auth types already validated
                },
                None => {
                    ctx.route_config.as_ref()
                        .map(|cfg| crate::gateway::authz::authorize(identity, &cfg.auth))
                        .unwrap_or(true)
                }
            };
            if !authorized {
                tracing::warn!(user = %identity.sub, "authorization denied");
                self.gateway.metrics.authz_denied_total.inc();
                return crate::gateway::errors::send_gateway_error(
                    session,
                    403,
                    "forbidden",
                    "user lacks required role",
                )
                .await;
            }
        }

        // 5. Rate limiting (existing node-local, enhanced with distributed)
        // Use endpoint-level rate limit if present, otherwise route default
        let effective_rate_limit = endpoint_rule
            .and_then(|e| e.rate_limit.clone())
            .or_else(|| ctx.route_config.as_ref().and_then(|c| c.rate_limit.clone()));

        if let Some(app_id) = &ctx.app_id {
            let source_ip = session
                .client_addr()
                .and_then(|addr| addr.as_inet().map(|inet| inet.ip()))
                .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));

            // Check per-app and per-IP rate limits (existing local limiter)
            match self.rate_limiter.check_request(&app_id.0, source_ip) {
                Ok(()) => {}
                Err(e) => {
                    let reason = match e {
                        crate::rate_limiter::RateLimitDenied::AppLimitExceeded { .. } => {
                            "app_limit"
                        }
                        crate::rate_limiter::RateLimitDenied::IpLimitExceeded { .. } => "ip_limit",
                    };
                    tracing::warn!(app_id = %app_id.0, %source_ip, reason = %e, "rate limit exceeded");
                    if let Some(ref metrics) = self.metrics {
                        metrics.record_rejection(&app_id.0, reason);
                    }
                    session.set_keepalive(None);
                    session.respond_error(429).await?;
                    return Ok(true);
                }
            }

            // Check distributed rate limiter if configured
            if let Some(ref route_rl) = effective_rate_limit {
                if route_rl.distributed {
                    let limiters = self.gateway.distributed_limiters.read().await;
                    if let Some(limiter) = limiters.get(&app_id.0) {
                        if !limiter.check_request().await {
                            self.gateway.metrics.rate_limit_denied_total.inc();
                            return crate::gateway::errors::send_gateway_error(
                                session,
                                429,
                                "rate_limit_exceeded",
                                &format!("app rate limit: {} req/s", route_rl.requests_per_second),
                            )
                            .await;
                        }
                    }
                }
            }
        }

        // 6. Circuit breaker (new)
        if let Some(ref app_id) = ctx.app_id {
            if self.gateway.is_circuit_open(app_id) {
                self.gateway.metrics.circuit_breaker_rejected_total.inc();
                return crate::gateway::errors::send_gateway_error(
                    session,
                    503,
                    "circuit_open",
                    "upstream is unhealthy, retry later",
                )
                .await;
            }
        }

        // 7. Backpressure (existing)
        if !self.backpressure.is_accepting() {
            tracing::warn!("node at capacity, rejecting request");
            if let Some(ref metrics) = self.metrics {
                metrics.record_rejection("*", "backpressure");
            }
            session.set_keepalive(None);
            session.respond_error(503).await?;
            return Ok(true);
        }

        Ok(false) // false = do NOT abort the request
    }

    /// Step 2: Select the upstream (with cold-start if needed).
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        let app_id = ctx
            .app_id
            .as_ref()
            .ok_or_else(|| pingora_core::Error::new_str("no app for this host"))?;

        let addr = self
            .select_upstream(app_id)
            .await
            .ok_or_else(|| pingora_core::Error::new_str("failed to select upstream"))?;

        ctx.upstream_addr = Some(addr);
        Ok(Box::new(HttpPeer::new(
            addr,
            false, // not TLS to upstream (internal)
            app_id.0.clone(),
        )))
    }

    /// Step 3 (optional): Modify request headers before forwarding.
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        if let Some(id) = &ctx.app_id {
            let _ = upstream_request.insert_header("X-App-Id", &id.0);
        }

        // Propagate distributed tracing context to upstream.
        if let Some(ref tid) = ctx.trace_id {
            let _ = upstream_request.insert_header("X-Trace-Id", tid.as_str());
        }
        if let Some(tp) = session
            .req_header()
            .headers
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
        {
            let _ = upstream_request.insert_header("traceparent", tp);
        }
        if let Some(ts) = session
            .req_header()
            .headers
            .get("tracestate")
            .and_then(|v| v.to_str().ok())
        {
            let _ = upstream_request.insert_header("tracestate", ts);
        }

        // Strip the matched path prefix if the route is configured for it.
        if ctx.strip_prefix {
            if let Some(ref prefix) = ctx.matched_prefix {
                let original_uri = upstream_request.uri.to_string();
                if let Some(stripped) = original_uri.strip_prefix(prefix) {
                    let new_path = if stripped.starts_with('/') || stripped.is_empty() {
                        stripped.to_string()
                    } else {
                        format!("/{}", stripped)
                    };
                    let _ = upstream_request
                        .insert_header("X-Forwarded-Prefix", prefix.as_str())
                        .map(|_| ());
                    let _ = upstream_request
                        .insert_header("X-Original-Uri", &original_uri)
                        .map(|_| ());
                    let new_uri = if let Some(query) = upstream_request.uri.query() {
                        format!("{}?{}", new_path, query)
                    } else {
                        new_path
                    };
                    if let Ok(parsed) = new_uri.parse() {
                        upstream_request.uri = parsed;
                    }
                }
            }
        }

        // 7. Request Transformation (new)
        if let Some(ref cfg) = ctx.route_config {
            if let Some(ref transform) = cfg.transform {
                crate::gateway::transform::apply_request_transform(
                    upstream_request,
                    transform,
                    ctx.user_identity.as_ref(),
                );
            }
        }

        Ok(())
    }

    /// Step 4 (optional): Modify response headers from upstream.
    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut pingora::http::ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // Add CORS headers to normal responses
        if let Some(ref cfg) = ctx.route_config {
            if cfg.cors.is_some() {
                let origin = _session
                    .req_header()
                    .headers
                    .get("origin")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                crate::gateway::cors::add_cors_headers(upstream_response, cfg, origin);
            }
        }

        // Record circuit breaker success/failure based on response status
        if let Some(ref app_id) = ctx.app_id {
            let status = upstream_response.status.as_u16();
            if (500..600).contains(&status) {
                self.gateway.circuit_breaker.record_failure(&app_id.0);
            } else {
                self.gateway.circuit_breaker.record_success(&app_id.0);
            }
        }

        Ok(())
    }

    /// Step 5 (optional): Log after the response is sent.
    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_core::Error>,
        ctx: &mut Self::CTX,
    ) {
        let latency_ms = ctx.start.elapsed().as_millis();
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);

        // Update circuit breaker metrics for open circuits
        self.gateway.metrics.circuits_open.set(self.gateway.circuit_breaker.open_circuit_count());

        tracing::info!(
            app_id = ctx
                .app_id
                .as_ref()
                .map(|a| a.0.as_str())
                .unwrap_or("unknown"),
            status,
            latency_ms,
            "request completed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate_limiter::RateLimiter;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn test_unknown_host_returns_502_behavior() {
        let router = Arc::new(HostRouter::default());
        let resolved = router.resolve("unknown.com", "/").await;
        assert!(
            resolved.is_none(),
            "Unknown host should not resolve, leading to 502"
        );
    }

    #[tokio::test]
    async fn test_wasm_proxy_cold_start() {
        use crate::rate_limiter::RateLimitConfig;

        let router = Arc::new(HostRouter::default());
        let upstream = Arc::new(UpstreamRegistry::default());
        let rate_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));

        let cold_start_triggered = Arc::new(AtomicBool::new(false));
        let cold_start_triggered_clone = cold_start_triggered.clone();

        let cold_start = Arc::new(move |_app_id: AppId| {
            let trigger = cold_start_triggered_clone.clone();
            Box::pin(async move {
                trigger.store(true, Ordering::SeqCst);
                Some("127.0.0.1:8080".parse().unwrap())
            }) as futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
        });

        let proxy = WasmProxy {
            router,
            upstream,
            rate_limiter,
            backpressure: crate::backpressure::BackpressureSignal::new(),
            node_table: Arc::new(crate::node_table::NodeLoadTable::default()),
            metrics: None,
            gateway: Arc::new(Gateway::new(None)),
            cold_start,
        };

        let app_id = AppId("test-app".to_string());

        let addr = proxy.upstream.next(&app_id).await;
        assert!(addr.is_none());

        let result = (proxy.cold_start)(app_id.clone()).await;
        assert!(result.is_some());
        assert!(
            cold_start_triggered.load(Ordering::SeqCst),
            "Cold start should be triggered when the pool is empty"
        );
    }
}
