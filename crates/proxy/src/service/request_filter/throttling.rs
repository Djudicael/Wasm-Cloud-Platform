use pingora_core::Result as PingoraResult;
use pingora_proxy::Session;

use crate::service::{RequestCtx, WasmProxy};

pub(super) async fn apply_rate_limits(
    proxy: &WasmProxy,
    session: &mut Session,
    ctx: &RequestCtx,
    endpoint_rule: Option<&common::types::EndpointRule>,
) -> PingoraResult<bool> {
    let effective_rate_limit = endpoint_rule
        .and_then(|e| e.rate_limit.clone())
        .or_else(|| ctx.route_config.as_ref().and_then(|c| c.rate_limit.clone()));

    let Some(app_id) = &ctx.app_id else {
        return Ok(false);
    };

    let source_ip = session
        .client_addr()
        .and_then(|addr| addr.as_inet().map(|inet| inet.ip()))
        .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));

    match proxy.rate_limiter.check_request(&app_id.0, source_ip) {
        Ok(()) => {}
        Err(e) => {
            let reason = match e {
                crate::rate_limiter::RateLimitDenied::AppLimitExceeded { .. } => "app_limit",
                crate::rate_limiter::RateLimitDenied::IpLimitExceeded { .. } => "ip_limit",
            };
            tracing::warn!(app_id = %app_id.0, %source_ip, reason = %e, "rate limit exceeded");
            if let Some(ref metrics) = proxy.metrics {
                metrics.record_rejection(&app_id.0, reason);
            }
            session.set_keepalive(None);
            session.respond_error(429).await?;
            return Ok(true);
        }
    }

    if let Some(ref route_rl) = effective_rate_limit {
        if route_rl.distributed {
            let limiters = proxy.gateway.distributed_limiters.read().await;
            if let Some(limiter) = limiters.get(&app_id.0) {
                if !limiter.check_request().await {
                    proxy.gateway.metrics.rate_limit_denied_total.inc();
                    crate::gateway::errors::send_gateway_error(
                        session,
                        429,
                        "rate_limit_exceeded",
                        &format!("app rate limit: {} req/s", route_rl.requests_per_second),
                    )
                    .await?;
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}

pub(super) async fn enforce_circuit_breaker(
    proxy: &WasmProxy,
    session: &mut Session,
    ctx: &RequestCtx,
) -> PingoraResult<bool> {
    if let Some(ref app_id) = ctx.app_id {
        if proxy.gateway.is_circuit_open(app_id) {
            proxy.gateway.metrics.circuit_breaker_rejected_total.inc();
            crate::gateway::errors::send_gateway_error(
                session,
                503,
                "circuit_open",
                "upstream is unhealthy, retry later",
            )
            .await?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn enforce_backpressure(
    proxy: &WasmProxy,
    session: &mut Session,
) -> PingoraResult<bool> {
    if !proxy.backpressure.is_accepting() {
        tracing::warn!("node at capacity, rejecting request");
        if let Some(ref metrics) = proxy.metrics {
            metrics.record_rejection("*", "backpressure");
        }
        session.set_keepalive(None);
        session.respond_error(503).await?;
        return Ok(true);
    }
    Ok(false)
}
