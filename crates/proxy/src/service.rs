use super::{
    backpressure::BackpressureSignal, metrics::RateLimitMetrics, node_table::NodeLoadTable,
    rate_limiter::RateLimiter, router::HostRouter, upstream::UpstreamRegistry,
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
}

/// The main Pingora proxy service.
pub struct WasmProxy {
    pub router: Arc<HostRouter>,
    pub upstream: Arc<UpstreamRegistry>,
    pub rate_limiter: Arc<RateLimiter>,
    pub backpressure: BackpressureSignal,
    pub node_table: Arc<NodeLoadTable>,
    pub metrics: Option<Arc<RateLimitMetrics>>,
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
        tracing::info!(app = %app_id.0, "cold start on local node");
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
        // If the incoming request carries a W3C traceparent header, we parse
        // the trace-id from it; otherwise we generate a random one so every
        // request has a correlatable identifier in upstream logs.
        let trace_id = session
            .req_header()
            .headers
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
            .and_then(|tp| {
                // W3C traceparent format: version-trace_id-parent_id-flags
                // e.g. "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                let parts: Vec<&str> = tp.split('-').collect();
                parts.get(1).map(|id| id.to_string())
            })
            .or_else(|| {
                // Generate a random 32-hex-char trace ID
                let bytes: [u8; 16] = rand::random();
                Some(hex::encode(bytes))
            });
        ctx.trace_id = trace_id.clone();

        let path = session.req_header().uri.path().to_string();
        let resolved = self.router.resolve(&host, &path).await;
        if let Some(r) = &resolved {
            ctx.app_id = Some(r.app_id.clone());
            ctx.strip_prefix = r.strip_prefix;
            ctx.matched_prefix = Some(r.matched_prefix.clone());
        }
        if ctx.app_id.is_none() {
            tracing::warn!(host, path, "no route found for host+path");
            session.respond_error(502).await?;
            return Ok(true);
        }

        // Check backpressure first (node at capacity)
        if !self.backpressure.is_accepting() {
            tracing::warn!("node at capacity, rejecting request");
            if let Some(ref metrics) = self.metrics {
                metrics.record_rejection("*", "backpressure");
            }
            session.set_keepalive(None);
            session.respond_error(503).await?;
            return Ok(true); // true = abort the request
        }

        // Check per-app and per-IP rate limits
        if let Some(app_id) = &ctx.app_id {
            // Extract source IP from session
            let source_ip = session
                .client_addr()
                .and_then(|addr| addr.as_inet().map(|inet| inet.ip()))
                .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));

            match self.rate_limiter.check_request(&app_id.0, source_ip) {
                Ok(()) => {
                    // Rate limit passed, continue
                }
                Err(e) => {
                    let reason = match e {
                        crate::rate_limiter::RateLimitDenied::AppLimitExceeded { .. } => {
                            "app_limit"
                        }
                        crate::rate_limiter::RateLimitDenied::IpLimitExceeded { .. } => "ip_limit",
                    };
                    tracing::warn!(app = %app_id.0, %source_ip, reason = %e, "rate limit exceeded");
                    if let Some(ref metrics) = self.metrics {
                        metrics.record_rejection(&app_id.0, reason);
                    }
                    session.set_keepalive(None);
                    session.respond_error(429).await?;
                    return Ok(true); // true = abort the request
                }
            }
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
        // 1. Always inject X-Trace-Id so the upstream app can correlate logs.
        // 2. If the downstream request had a traceparent header, forward it
        //    unchanged so the W3C trace context is preserved across hops.
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
        // Also propagate tracestate if present (W3C companion to traceparent)
        if let Some(ts) = session
            .req_header()
            .headers
            .get("tracestate")
            .and_then(|v| v.to_str().ok())
        {
            let _ = upstream_request.insert_header("tracestate", ts);
        }
        // Strip the matched path prefix if the route is configured for it.
        // e.g. route "/api" with strip_prefix=true: /api/users → /users
        //
        // We set X-Forwarded-Prefix so the downstream app knows what was
        // stripped, and rewrite the URI path. Pingora's RequestHeader.uri
        // is a std http::Uri; we reconstruct it from parts.
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
                    // Rebuild URI: preserve query string if present
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
        Ok(())
    }

    /// Step 4 (optional): Log after the response is sent.
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
        tracing::info!(
            app = ctx
                .app_id
                .as_ref()
                .map(|a| a.0.as_str())
                .unwrap_or("unknown"),
            status,
            latency_ms,
            "request completed"
        );
        // TODO: push to metrics channel
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
        // If a route doesn't exist, it returns None.
        // In request_filter, this leaves ctx.app_id as None, which causes upstream_peer to return an error (502).
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
            metrics: None, // No metrics in test
            cold_start,
        };

        let app_id = AppId("test-app".to_string());

        // No upstreams added yet, so next() returns None
        let addr = proxy.upstream.next(&app_id).await;
        assert!(addr.is_none());

        // This simulates the behavior in upstream_peer when next() is None
        let result = (proxy.cold_start)(app_id.clone()).await;
        assert!(result.is_some());
        assert!(
            cold_start_triggered.load(Ordering::SeqCst),
            "Cold start should be triggered when the pool is empty"
        );
    }
}
