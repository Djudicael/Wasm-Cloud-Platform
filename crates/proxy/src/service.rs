use super::{
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
}

/// The main Pingora proxy service.
pub struct WasmProxy {
    pub router: Arc<HostRouter>,
    pub upstream: Arc<UpstreamRegistry>,
    pub rate_limiter: Arc<RateLimiter>,
    pub node_table: Arc<NodeLoadTable>,
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

        ctx.app_id = self.router.resolve(&host).await;
        if ctx.app_id.is_none() {
            tracing::warn!(host, "no route found for host");
            // Will result in a 502 from Pingora
        }

        if let Some(app_id) = &ctx.app_id {
            if !self.rate_limiter.allow(&app_id.0).await {
                session.set_keepalive(None);
                session.respond_error(429).await?;
                return Ok(true); // true = abort the request
            }
        }

        Ok(false) // false = do NOT abort the request
    }

    /// Step 2: Select the upstream (with cold-start if needed).
    async fn upstream_peer(
        &self,
        session: &mut Session,
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
        _session: &mut Session,
        upstream_request: &mut pingora_http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        if let Some(id) = &ctx.app_id {
            // pingora_http RequestHeader provides insert_header functionality
            let _ = upstream_request.insert_header("X-App-Id", &id.0);
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
        let resolved = router.resolve("unknown.com").await;
        assert!(
            resolved.is_none(),
            "Unknown host should not resolve, leading to 502"
        );
    }

    #[tokio::test]
    async fn test_wasm_proxy_cold_start() {
        let router = Arc::new(HostRouter::default());
        let upstream = Arc::new(UpstreamRegistry::default());
        let rate_limiter = Arc::new(RateLimiter::new(10.0, 10.0));

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
            node_table: Arc::new(crate::node_table::NodeLoadTable::default()),
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
