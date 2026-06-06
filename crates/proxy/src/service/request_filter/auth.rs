use pingora_core::Result as PingoraResult;
use pingora_proxy::Session;

use crate::service::{RequestCtx, WasmProxy};

pub(super) async fn resolve_effective_auth(
    proxy: &WasmProxy,
    session: &mut Session,
    ctx: &RequestCtx,
    endpoint_rule: Option<&common::types::EndpointRule>,
) -> PingoraResult<Option<crate::gateway::config::AuthPolicy>> {
    Ok(Some(match endpoint_rule {
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
                        crate::gateway::errors::send_gateway_error(
                            session,
                            401,
                            "unauthorized",
                            "invalid X-Api-Key",
                        )
                        .await?;
                        return Ok(None);
                    }
                    crate::gateway::config::AuthPolicy::None
                } else {
                    crate::gateway::errors::send_gateway_error(
                        session,
                        401,
                        "unauthorized",
                        "missing X-Api-Key header",
                    )
                    .await?;
                    return Ok(None);
                }
            }
        },
        None => ctx
            .route_config
            .as_ref()
            .map(|c| c.auth.clone())
            .unwrap_or(crate::gateway::config::AuthPolicy::None),
    }))
}

pub(super) async fn authenticate_request(
    proxy: &WasmProxy,
    session: &mut Session,
    ctx: &mut RequestCtx,
    effective_auth: &crate::gateway::config::AuthPolicy,
) -> PingoraResult<bool> {
    if *effective_auth == crate::gateway::config::AuthPolicy::None {
        return Ok(false);
    }

    match proxy
        .gateway
        .authenticate_with_policy(session, effective_auth)
        .await
    {
        Ok(identity) => {
            proxy.gateway.metrics.auth_success_total.inc();
            ctx.user_identity = Some(identity);
            Ok(false)
        }
        Err(e) => {
            tracing::warn!(error = %e, "authentication failed");
            proxy.gateway.metrics.auth_failure_total.inc();
            crate::gateway::errors::send_gateway_error(
                session,
                401,
                "unauthorized",
                "missing or invalid JWT token",
            )
            .await?;
            Ok(true)
        }
    }
}

pub(super) async fn authorize_request(
    proxy: &WasmProxy,
    session: &mut Session,
    ctx: &RequestCtx,
    endpoint_rule: Option<&common::types::EndpointRule>,
) -> PingoraResult<bool> {
    let Some(identity) = ctx.user_identity.as_ref() else {
        return Ok(false);
    };

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
        crate::gateway::errors::send_gateway_error(
            session,
            403,
            "forbidden",
            "user lacks required role or scope",
        )
        .await?;
        return Ok(true);
    }
    Ok(false)
}
