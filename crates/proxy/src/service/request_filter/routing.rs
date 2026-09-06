use opentelemetry::global;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::TraceContextExt as _;
use pingora_core::Result as PingoraResult;
use pingora_proxy::Session;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::service::{RequestCtx, WasmProxy};

struct HeaderExtractor<'a>(&'a http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

pub(super) fn initialize_request_span(
    session: &Session,
    host: &str,
    path: &str,
    ctx: &mut RequestCtx,
) {
    let method = session.req_header().method.as_str();
    let span = tracing::info_span!(
        "http.server.request",
        otel.name = %format!("{method} {path}"),
        otel.kind = "server",
        http.request.method = %method,
        url.path = %path,
        server.address = %host,
        http.response.status_code = tracing::field::Empty,
        app_id = tracing::field::Empty,
        trace_id = tracing::field::Empty,
    );
    let parent = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(&session.req_header().headers))
    });
    let _ = span.set_parent(parent);

    let span_context = span.context();
    let trace_id = span_context.span().span_context().trace_id();
    let trace_id = if trace_id == opentelemetry::trace::TraceId::INVALID {
        let bytes: [u8; 16] = rand::random();
        hex::encode(bytes)
    } else {
        trace_id.to_string()
    };
    span.record("trace_id", trace_id.as_str());
    ctx.trace_id = Some(trace_id);
    ctx.request_span = span;
}

pub(super) async fn resolve_route(
    proxy: &WasmProxy,
    normalized_host: &str,
    path: &str,
    ctx: &mut RequestCtx,
) {
    let resolved = proxy.router.resolve(normalized_host, path).await;
    let route_config = match &resolved {
        Some(r) => proxy.gateway.get_route_config(&r.app_id).await,
        None => None,
    };

    if let Some(r) = &resolved {
        ctx.app_id = Some(r.app_id.clone());
        ctx.request_span.record("app_id", r.app_id.0.as_str());
        ctx.strip_prefix = r.strip_prefix;
        ctx.matched_prefix = Some(r.matched_prefix.clone());
    }
    ctx.route_config = route_config;
}

pub(super) fn find_endpoint_rule<'a>(
    ctx: &'a RequestCtx,
    path: &str,
    method: &str,
) -> Option<&'a common::types::EndpointRule> {
    ctx.route_config.as_ref().and_then(|cfg| {
        cfg.endpoints.iter().find(|e| {
            path.starts_with(&e.path)
                && (e.methods.is_empty()
                    || e.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
        })
    })
}

pub(super) async fn handle_cors_preflight(
    proxy: &WasmProxy,
    session: &mut Session,
    ctx: &RequestCtx,
) -> PingoraResult<bool> {
    if let Some(ref cfg) = ctx.route_config {
        if cfg.cors.is_some() && session.req_header().method == "OPTIONS" {
            proxy.gateway.metrics.cors_preflight_total.inc();
            return crate::gateway::cors::handle_cors_preflight(session, cfg).await;
        }
    }
    Ok(false)
}
