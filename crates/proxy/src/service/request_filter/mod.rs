//! Request admission and gateway policy evaluation.
//!
//! Pingora calls this stage before upstream selection. It resolves the route,
//! applies gateway auth/authz/CORS/rate-limit policy, and decides whether the
//! request should continue down the proxy pipeline.

mod auth;
mod routing;
mod throttling;

use pingora_core::Result as PingoraResult;
use pingora_proxy::Session;

use super::{RequestCtx, WasmProxy};
use auth::{authenticate_request, authorize_request, resolve_effective_auth};
use routing::{find_endpoint_rule, handle_cors_preflight, initialize_request_span, resolve_route};
use throttling::{apply_rate_limits, enforce_backpressure, enforce_circuit_breaker};

pub(super) async fn request_filter(
    proxy: &WasmProxy,
    session: &mut Session,
    ctx: &mut RequestCtx,
) -> PingoraResult<bool> {
    let host = session
        .req_header()
        .headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let normalized_host = super::extract_request_host(session);
    let path = session.req_header().uri.path().to_string();

    if normalized_host.is_empty() || path == "/_platform/health" {
        return Ok(false);
    }

    initialize_request_span(session, &normalized_host, &path, ctx);

    if let Some(content_length) = session
        .req_header()
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if content_length > proxy.max_body_size_bytes {
            tracing::warn!(
                content_length,
                max = proxy.max_body_size_bytes,
                "request body too large, rejecting"
            );
            session.respond_error(413).await?;
            return Ok(true);
        }
    }

    resolve_route(proxy, &normalized_host, &path, ctx).await;

    if ctx.app_id.is_none() {
        tracing::warn!(host, normalized_host, path, "no route found for host+path");
        session.respond_error(502).await?;
        return Ok(true);
    }

    if handle_cors_preflight(proxy, session, ctx).await? {
        return Ok(true);
    }

    let endpoint_rule =
        find_endpoint_rule(ctx, &path, session.req_header().method.as_str()).cloned();
    let Some(effective_auth) =
        resolve_effective_auth(proxy, session, ctx, endpoint_rule.as_ref()).await?
    else {
        return Ok(true);
    };
    if authenticate_request(proxy, session, ctx, &effective_auth).await? {
        return Ok(true);
    }
    if authorize_request(proxy, session, ctx, endpoint_rule.as_ref()).await? {
        return Ok(true);
    }
    if apply_rate_limits(proxy, session, ctx, endpoint_rule.as_ref()).await? {
        return Ok(true);
    }
    if enforce_circuit_breaker(proxy, session, ctx).await? {
        return Ok(true);
    }
    if enforce_backpressure(proxy, session).await? {
        return Ok(true);
    }

    Ok(false)
}
