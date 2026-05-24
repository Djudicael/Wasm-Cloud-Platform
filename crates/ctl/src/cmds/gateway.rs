// crates/ctl/src/cmds/gateway.rs
use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use common::types::{AppId, GatewayRouteConfig};
use messaging::{events::Event, NatsBus};

#[derive(Args)]
pub struct GatewayArgs {
    #[command(subcommand)]
    pub action: GatewayAction,
}

#[derive(Subcommand)]
pub enum GatewayAction {
    /// Set authentication policy for a route
    SetAuth {
        app_id: String,
        #[arg(long, default_value = "none")]
        policy: String,
        #[arg(long, value_delimiter = ',')]
        roles: Vec<String>,
        #[arg(long)]
        client_id: Option<String>,
    },
    /// Set CORS policy for a route
    SetCors {
        app_id: String,
        #[arg(long, value_delimiter = ',')]
        origins: Vec<String>,
        #[arg(long)]
        credentials: bool,
        #[arg(long, default_value = "86400")]
        max_age: u32,
    },
    /// Set rate limit for a route
    SetRateLimit {
        app_id: String,
        #[arg(long)]
        rps: u32,
        #[arg(long)]
        burst: u32,
        #[arg(long)]
        distributed: bool,
    },
    /// Set circuit breaker config for a route
    SetCircuitBreaker {
        app_id: String,
        #[arg(long, default_value = "5")]
        failure_threshold: u32,
        #[arg(long, default_value = "30")]
        reset_timeout: u32,
    },
    /// Show gateway config for a route
    Show { app_id: String },
    /// Remove gateway config for a route (revert to public)
    Reset { app_id: String },
}

pub async fn run(
    args: GatewayArgs,
    bus: &NatsBus,
    node_api: &str,
    http: &reqwest::Client,
) -> Result<()> {
    match args.action {
        GatewayAction::SetAuth {
            app_id,
            policy,
            roles,
            client_id,
        } => {
            let app_id = parse_app_id(&app_id)?;
            let auth = match policy.as_str() {
                "none" => common::types::AuthPolicy::None,
                "authenticated" => common::types::AuthPolicy::Authenticated,
                "roles" => common::types::AuthPolicy::Roles {
                    allowed_roles: roles,
                    client_id,
                },
                other => anyhow::bail!("unknown auth policy: {}", other),
            };
            let config = GatewayRouteConfig {
                auth,
                ..Default::default()
            };
            publish_config_update(bus, app_id, config).await
        }
        GatewayAction::SetCors {
            app_id,
            origins,
            credentials,
            max_age,
        } => {
            let app_id = parse_app_id(&app_id)?;
            let cors = common::types::CorsPolicy {
                allowed_origins: origins,
                allowed_methods: common::types::CorsPolicy::default_methods(),
                allowed_headers: common::types::CorsPolicy::default_headers(),
                expose_headers: vec![],
                allow_credentials: credentials,
                max_age_secs: max_age,
            };
            let config = GatewayRouteConfig {
                cors: Some(cors),
                ..Default::default()
            };
            publish_config_update(bus, app_id, config).await
        }
        GatewayAction::SetRateLimit {
            app_id,
            rps,
            burst,
            distributed,
        } => {
            let app_id = parse_app_id(&app_id)?;
            let rate_limit = common::types::RouteRateLimit {
                requests_per_second: rps,
                burst_capacity: burst,
                distributed,
            };
            let config = GatewayRouteConfig {
                rate_limit: Some(rate_limit),
                ..Default::default()
            };
            publish_config_update(bus, app_id, config).await
        }
        GatewayAction::SetCircuitBreaker {
            app_id,
            failure_threshold,
            reset_timeout,
        } => {
            let app_id = parse_app_id(&app_id)?;
            let cb = common::types::CircuitBreakerConfig {
                failure_threshold,
                reset_timeout_secs: reset_timeout,
                failure_criteria: common::types::FailureCriteria::ServerErrors,
            };
            let config = GatewayRouteConfig {
                circuit_breaker: Some(cb),
                ..Default::default()
            };
            publish_config_update(bus, app_id, config).await
        }
        GatewayAction::Show { app_id } => {
            let url = format!("{}/admin/gateway/{}", node_api, app_id);
            let resp = http.get(&url).send().await?;
            if resp.status().is_success() {
                let body: serde_json::Value = resp.json().await?;
                println!("{}", serde_json::to_string_pretty(&body)?);
            } else {
                anyhow::bail!("Failed to fetch gateway config: {}", resp.status());
            }
            Ok(())
        }
        GatewayAction::Reset { app_id } => {
            let app_id = parse_app_id(&app_id)?;
            let event = Event::GatewayConfigRemove {
                app_id: app_id.clone(),
            };
            bus.publish(&event).await?;
            println!(
                "{} Gateway config removed for {}",
                "✓".green(),
                app_id.0.cyan()
            );
            Ok(())
        }
    }
}

fn parse_app_id(s: &str) -> Result<AppId> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        anyhow::bail!("app_id must be <name>:<version>");
    }
    Ok(AppId::new(parts[0], parts[1]))
}

async fn publish_config_update(
    bus: &NatsBus,
    app_id: AppId,
    config: GatewayRouteConfig,
) -> Result<()> {
    let event = Event::GatewayConfigUpdate {
        app_id: app_id.clone(),
        config,
    };
    bus.publish(&event).await?;
    println!(
        "{} Gateway config updated for {}",
        "✓".green(),
        app_id.0.cyan()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{GatewayAction, GatewayArgs};
    use clap::Parser;

    #[derive(Parser)]
    struct GatewayCliTestHarness {
        #[command(flatten)]
        args: GatewayArgs,
    }

    #[test]
    fn test_set_rate_limit_defaults_to_node_local() {
        let parsed = GatewayCliTestHarness::parse_from([
            "wasm-ctl",
            "set-rate-limit",
            "api:v1",
            "--rps",
            "100",
            "--burst",
            "20",
        ]);

        match parsed.args.action {
            GatewayAction::SetRateLimit { distributed, .. } => assert!(!distributed),
            other => panic!(
                "expected SetRateLimit, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_set_rate_limit_requires_explicit_distributed_opt_in() {
        let parsed = GatewayCliTestHarness::parse_from([
            "wasm-ctl",
            "set-rate-limit",
            "api:v1",
            "--rps",
            "100",
            "--burst",
            "20",
            "--distributed",
        ]);

        match parsed.args.action {
            GatewayAction::SetRateLimit { distributed, .. } => assert!(distributed),
            other => panic!(
                "expected SetRateLimit, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }
}
