//! Upstream selection and request/response adaptation.
//!
//! These helpers run after `request_filter` accepts the request. They choose an
//! upstream, shape the forwarded request, adjust the response, and emit the
//! final request log record.

use opentelemetry::global;
use opentelemetry::propagation::Injector;
use opentelemetry::trace::TraceContextExt as _;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result as PingoraResult;
use pingora_proxy::Session;

use super::{strip_uri_prefix, RequestCtx, WasmProxy};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

struct RequestHeaderInjector<'a>(&'a mut RequestHeader);

impl Injector for RequestHeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        let Ok(name) = http::HeaderName::from_bytes(key.as_bytes()) else {
            return;
        };
        let Ok(value) = http::HeaderValue::from_str(&value) else {
            return;
        };
        let _ = self.0.insert_header(name, value);
    }
}

pub(super) async fn upstream_peer(
    proxy: &WasmProxy,
    ctx: &mut RequestCtx,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = ctx
        .app_id
        .as_ref()
        .ok_or_else(|| pingora_core::Error::new_str("no app for this host"))?;

    let endpoint = proxy
        .select_upstream(app_id)
        .await
        .ok_or_else(|| pingora_core::Error::new_str("failed to select upstream"))?;

    ctx.upstream_addr = Some(endpoint.addr);
    let mut peer = HttpPeer::new(endpoint.addr, false, app_id.0.clone());
    if endpoint.h2c {
        peer.options.set_http_version(2, 2);
    }
    Ok(Box::new(peer))
}

pub(super) async fn upstream_request_filter(
    session: &mut Session,
    upstream_request: &mut RequestHeader,
    ctx: &mut RequestCtx,
) -> PingoraResult<()> {
    if let Some(id) = &ctx.app_id {
        let _ = upstream_request.insert_header("X-App-Id", &id.0);
    }

    if let Some(ref tid) = ctx.trace_id {
        let _ = upstream_request.insert_header("X-Trace-Id", tid.as_str());
    }
    let telemetry_context = ctx.request_span.context();
    if telemetry_context.span().span_context().is_valid() {
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(
                &telemetry_context,
                &mut RequestHeaderInjector(upstream_request),
            );
        });
    } else {
        for header in ["traceparent", "tracestate"] {
            if let Some(value) = session
                .req_header()
                .headers
                .get(header)
                .and_then(|value| value.to_str().ok())
            {
                let _ = upstream_request.insert_header(header, value);
            }
        }
    }

    if ctx.strip_prefix {
        if let Some(ref prefix) = ctx.matched_prefix {
            let original_uri = upstream_request.uri.to_string();
            let path = upstream_request.uri.path().to_string();
            let query = upstream_request.uri.query().map(str::to_string);
            if let Some(new_uri) = strip_uri_prefix(&path, query.as_deref(), prefix) {
                let _ = upstream_request
                    .insert_header("X-Forwarded-Prefix", prefix.as_str())
                    .map(|_| ());
                let _ = upstream_request
                    .insert_header("X-Original-Uri", &original_uri)
                    .map(|_| ());
                if let Ok(parsed) = new_uri.parse() {
                    upstream_request.uri = parsed;
                }
            }
        }
    }

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

pub(super) async fn upstream_response_filter(
    proxy: &WasmProxy,
    session: &mut Session,
    upstream_response: &mut ResponseHeader,
    ctx: &mut RequestCtx,
) -> PingoraResult<()> {
    if let Some(ref cfg) = ctx.route_config {
        if cfg.cors.is_some() {
            let origin = session
                .req_header()
                .headers
                .get("origin")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            crate::gateway::cors::add_cors_headers(upstream_response, cfg, origin);
        }
    }

    if let Some(ref app_id) = ctx.app_id {
        let status = upstream_response.status.as_u16();
        if (500..600).contains(&status) {
            proxy.gateway.circuit_breaker.record_failure(&app_id.0);
        } else {
            proxy.gateway.circuit_breaker.record_success(&app_id.0);
        }
    }

    Ok(())
}

pub(super) async fn logging(proxy: &WasmProxy, session: &mut Session, ctx: &mut RequestCtx) {
    let latency_ms = ctx.start.elapsed().as_millis();
    let status = session
        .response_written()
        .map(|r| r.status.as_u16())
        .unwrap_or(0);
    ctx.request_span.record("http.response.status_code", status);

    proxy
        .gateway
        .metrics
        .circuits_open
        .set(proxy.gateway.circuit_breaker.open_circuit_count());

    ctx.request_span.in_scope(|| {
        tracing::info!(
            app_id = ctx
                .app_id
                .as_ref()
                .map(|a| a.0.as_str())
                .unwrap_or("unknown"),
            trace_id = ctx.trace_id.as_deref().unwrap_or("unknown"),
            status,
            latency_ms,
            "request completed"
        );
    });
}
