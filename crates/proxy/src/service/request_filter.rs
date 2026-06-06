//! Request admission and gateway policy evaluation.
//!
//! Pingora calls this stage before upstream selection. It resolves the route,
//! applies gateway auth/authz/CORS/rate-limit policy, and decides whether the
//! request should continue down the proxy pipeline.

use pingora_core::Result as PingoraResult;
use pingora_proxy::Session;

use super::{extract_request_host, RequestCtx, WasmProxy};

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
    let normalized_host = extract_request_host(session);

    if normalized_host.is_empty() || session.req_header().uri.path() == "/_platform/health" {
        return Ok(false);
    }

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
    let resolved = proxy.router.resolve(&normalized_host, &path).await;

    let route_config = match &resolved {
        Some(r) => proxy.gateway.get_route_config(&r.app_id).await,
        None => None,
    };

    if let Some(r) = &resolved {
        ctx.app_id = Some(r.app_id.clone());
        ctx.strip_prefix = r.strip_prefix;
        ctx.matched_prefix = Some(r.matched_prefix.clone());
    }
    ctx.route_config = route_config.clone();

    if ctx.app_id.is_none() {
        tracing::warn!(host, normalized_host, path, "no route found for host+path");
        session.respond_error(502).await?;
        return Ok(true);
    }

    if let Some(ref cfg) = ctx.route_config {
        if cfg.cors.is_some() && session.req_header().method == "OPTIONS" {
            proxy.gateway.metrics.cors_preflight_total.inc();
            return crate::gateway::cors::handle_cors_preflight(session, cfg).await;
        }
    }

    let method = session.req_header().method.as_str();
    let endpoint_rule = ctx.route_config.as_ref().and_then(|cfg| {
        cfg.endpoints.iter().find(|e| {
            path.starts_with(&e.path)
                && (e.methods.is_empty()
                    || e.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
        })
    });

    let effective_auth = match endpoint_rule {
        Some(rule) => match &rule.auth {
            common::types::EndpointAuth::Inherit => ctx
                .route_config
                .as_ref()
                .map(|c| c.auth.clone())
                .unwrap_or(crate::gateway::config::AuthPolicy::None),
            common::types::EndpointAuth::None => crate::gateway::config::AuthPolicy::None,
            common::types::EndpointAuth::Authenticated => {
                crate::gateway::config::AuthPolicy::Authenticated
            }
            common::types::EndpointAuth::Roles {
                allowed_roles,
                client_id,
            } => crate::gateway::config::AuthPolicy::Roles {
                allowed_roles: allowed_roles.clone(),
                client_id: client_id.clone(),
            },
            common::types::EndpointAuth::ApiKey => {
                let api_key = session
                    .req_header()
                    .headers
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok());
                if let Some(key) = api_key {
                    let app_id = ctx.app_id.as_ref().map(|a| a.0.as_str()).unwrap_or("");
                    let path = session.req_header().uri.path();
                    if !proxy.gateway.validate_api_key(app_id, key, path).await {
                        return crate::gateway::errors::send_gateway_error(
                            session,
                            401,
                            "unauthorized",
                            "invalid X-Api-Key",
                        )
                        .await;
                    }
                    crate::gateway::config::AuthPolicy::None
                } else {
                    return crate::gateway::errors::send_gateway_error(
                        session,
                        401,
                        "unauthorized",
                        "missing X-Api-Key header",
                    )
                    .await;
                }
            }
        },
        None => ctx
            .route_config
            .as_ref()
            .map(|c| c.auth.clone())
            .unwrap_or(crate::gateway::config::AuthPolicy::None),
    };

    if effective_auth != crate::gateway::config::AuthPolicy::None {
        match proxy
            .gateway
            .authenticate_with_policy(session, &effective_auth)
            .await
        {
            Ok(identity) => {
                proxy.gateway.metrics.auth_success_total.inc();
                ctx.user_identity = Some(identity);
            }
            Err(e) => {
                tracing::warn!(error = %e, "authentication failed");
                proxy.gateway.metrics.auth_failure_total.inc();
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

    if let Some(ref identity) = ctx.user_identity {
        let authorized = match endpoint_rule {
            Some(rule) => {
                let roles_ok = match &rule.auth {
                    common::types::EndpointAuth::Roles {
                        allowed_roles,
                        client_id,
                    } => crate::gateway::authz::authorize_roles(
                        identity,
                        allowed_roles,
                        client_id.as_deref(),
                    ),
                    _ => true,
                };
                let scopes_ok =
                    crate::gateway::authz::authorize_scopes(identity, &rule.required_scopes);
                roles_ok && scopes_ok
            }
            None => ctx
                .route_config
                .as_ref()
                .map(|cfg| crate::gateway::authz::authorize(identity, &cfg.auth))
                .unwrap_or(true),
        };
        if !authorized {
            tracing::warn!(user = %identity.sub, "authorization denied");
            proxy.gateway.metrics.authz_denied_total.inc();
            return crate::gateway::errors::send_gateway_error(
                session,
                403,
                "forbidden",
                "user lacks required role or scope",
            )
            .await;
        }
    }

    let effective_rate_limit = endpoint_rule
        .and_then(|e| e.rate_limit.clone())
        .or_else(|| ctx.route_config.as_ref().and_then(|c| c.rate_limit.clone()));

    if let Some(app_id) = &ctx.app_id {
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

    if let Some(ref app_id) = ctx.app_id {
        if proxy.gateway.is_circuit_open(app_id) {
            proxy.gateway.metrics.circuit_breaker_rejected_total.inc();
            return crate::gateway::errors::send_gateway_error(
                session,
                503,
                "circuit_open",
                "upstream is unhealthy, retry later",
            )
            .await;
        }
    }

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
