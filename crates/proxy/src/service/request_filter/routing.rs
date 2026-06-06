use pingora_core::Result as PingoraResult;
use pingora_proxy::Session;

use crate::service::{RequestCtx, WasmProxy};

pub(super) fn extract_or_generate_trace_id(session: &Session) -> Option<String> {
    session
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
        })
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
