//! Pingora-facing proxy service implementation.
//!
//! The hot request path stays in this file. Pure request/host helpers and unit
//! tests live in submodules so the runtime behavior is easier to read.

mod forwarding;
mod helpers;
mod request_filter;
#[cfg(test)]
mod tests;
mod upstream_selection;

use super::{
    backpressure::BackpressureSignal, gateway::Gateway, metrics::RateLimitMetrics,
    node_table::NodeLoadTable, rate_limiter::RateLimiter, router::HostRouter,
    upstream::UpstreamRegistry,
};
use async_trait::async_trait;
use common::types::AppId;
#[cfg(test)]
use helpers::canonical_host;
use helpers::{extract_request_host, strip_uri_prefix};
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result as PingoraResult;
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;

/// Default maximum request body size in bytes (10 MB).
/// Requests with `Content-Length` exceeding this limit are rejected with 413.
pub const DEFAULT_MAX_BODY_SIZE_BYTES: usize = 10 * 1024 * 1024;
const REMOTE_STEER_FUEL_THRESHOLD_PERCENT: f32 = 80.0;

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
    /// Server span covering the complete Pingora request lifecycle.
    pub request_span: tracing::Span,

    // ── New gateway fields ──────────────────────────────────────
    /// Gateway configuration for the matched route.
    pub route_config: Option<crate::gateway::config::GatewayRouteConfig>,

    /// Authenticated user identity (set by auth middleware).
    pub user_identity: Option<crate::gateway::oidc::UserIdentity>,
}

/// The main Pingora proxy service — now with gateway capabilities.
#[derive(Clone)]
pub struct WasmProxy {
    pub router: Arc<HostRouter>,
    pub upstream: Arc<UpstreamRegistry>,
    pub rate_limiter: Arc<RateLimiter>,
    pub backpressure: BackpressureSignal,
    pub node_table: Arc<NodeLoadTable>,
    pub local_node_id: String,
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

    /// Maximum request body size in bytes. Requests with `Content-Length`
    /// exceeding this limit are rejected immediately with 413.
    /// Default: 10 MB (10 * 1024 * 1024 = 10_485_760).
    pub max_body_size_bytes: usize,
}

impl WasmProxy {
    async fn select_upstream(&self, app_id: &AppId) -> Option<crate::upstream::UpstreamEndpoint> {
        upstream_selection::select_upstream(self, app_id).await
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
            request_span: tracing::Span::none(),
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
        request_filter::request_filter(self, session, ctx).await
    }

    /// Step 2: Select the upstream (with cold-start if needed).
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        let _ = _session;
        forwarding::upstream_peer(self, ctx).await
    }

    /// Step 3 (optional): Modify request headers before forwarding.
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        let _ = self;
        forwarding::upstream_request_filter(session, upstream_request, ctx).await
    }

    /// Step 4 (optional): Modify response headers from upstream.
    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut pingora::http::ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        forwarding::upstream_response_filter(self, _session, upstream_response, ctx).await
    }

    /// Step 5 (optional): Log after the response is sent.
    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_core::Error>,
        ctx: &mut Self::CTX,
    ) {
        let _ = _e;
        forwarding::logging(self, session, ctx).await;
    }
}
