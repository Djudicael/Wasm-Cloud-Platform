use super::args::DeployArgs;
use anyhow::Result;
use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
use std::collections::HashMap;

type DeployPayload = (
    AppConfig,
    Option<common::types::GatewayRouteConfig>,
    Vec<common::types::Route>,
    Vec<common::types::ApiKeyRecord>,
);

pub(super) fn build_deploy_payload(
    args: &DeployArgs,
    manifest: Option<&super::super::manifest::DeployManifest>,
    app_id: &AppId,
    namespace: &str,
) -> Result<DeployPayload> {
    Ok(if let Some(manifest) = manifest {
        let mut config = manifest.to_app_config();
        config.id = app_id.clone();
        if args.fuel != 500_000_000 {
            config.fuel_quota = FuelQuota(args.fuel);
        }
        if args.memory_mb != 128 {
            config.memory_limit = MemoryPages(args.memory_mb * 16);
        }
        if args.max_instances != 10 {
            config.max_instances = args.max_instances;
        }
        if args.idle_timeout != 300 {
            config.idle_timeout_secs = args.idle_timeout;
        }
        if !args.env_vars.is_empty() {
            for (k, v) in &args.env_vars {
                config.env_vars.insert(k.clone(), v.clone());
            }
        }
        if !args.secret_keys.is_empty() {
            config.secret_keys = args.secret_keys.clone();
        }
        let policy = build_policy_config(args)?;
        if policy.is_some() {
            config.policy = policy;
        }
        (
            config,
            manifest.to_gateway_config(),
            manifest.to_routes(app_id)?,
            manifest.api_keys.clone(),
        )
    } else {
        let policy = build_policy_config(args)?;
        let gateway_config = build_gateway_config(args);
        let config = AppConfig {
            id: app_id.clone(),
            fuel_quota: FuelQuota(args.fuel),
            memory_limit: MemoryPages(args.memory_mb * 16),
            max_instances: args.max_instances,
            idle_timeout_secs: args.idle_timeout,
            wasm_bind_port: 8080,
            env_vars: args.env_vars.clone().into_iter().collect::<HashMap<_, _>>(),
            secret_keys: args.secret_keys.clone(),
            extended_limits: None,
            health_check_path: None,
            db_max_connections: None,
            rate_limit: None,
            tenant_id: None,
            policy,
            namespace: namespace.to_string(),
            placement: common::types::PlacementPolicy::EveryNode,
            local_dependencies: Vec::new(),
        };
        (config, gateway_config, Vec::new(), Vec::new())
    })
}

pub(super) fn build_gateway_config(args: &DeployArgs) -> Option<common::types::GatewayRouteConfig> {
    let mut config = common::types::GatewayRouteConfig::default();
    let mut has_config = false;

    if let Some(ref auth) = args.gateway_auth {
        config.auth = match auth.as_str() {
            "none" => common::types::AuthPolicy::None,
            "authenticated" => common::types::AuthPolicy::Authenticated,
            "roles" => common::types::AuthPolicy::Roles {
                allowed_roles: args.gateway_roles.clone(),
                client_id: args.gateway_oidc_client.clone(),
            },
            _ => common::types::AuthPolicy::None,
        };
        has_config = true;
    }

    if !args.gateway_cors_origins.is_empty() {
        config.cors = Some(common::types::CorsPolicy {
            allowed_origins: args.gateway_cors_origins.clone(),
            allowed_methods: common::types::CorsPolicy::default_methods(),
            allowed_headers: common::types::CorsPolicy::default_headers(),
            expose_headers: vec![],
            allow_credentials: args.gateway_cors_credentials,
            max_age_secs: 86400,
        });
        has_config = true;
    }

    if args.gateway_rps.is_some() || args.gateway_rps_burst.is_some() {
        config.rate_limit = Some(common::types::RouteRateLimit {
            requests_per_second: args.gateway_rps.unwrap_or(1000),
            burst_capacity: args.gateway_rps_burst.unwrap_or(50),
            distributed: args.gateway_rps_distributed,
        });
        has_config = true;
    }

    if has_config {
        Some(config)
    } else {
        None
    }
}

/// Build a `PolicyConfig` from the CLI flags.
///
/// If `--policy-profile` is given, start from that profile and overlay any
/// explicit `--policy-network-*` / `--policy-fs-*` overrides on top.
/// If no profile is given but individual flags are present, build a config
/// with only the specified overrides (defaults fill in on `resolve()`).
pub(super) fn build_policy_config(
    args: &DeployArgs,
) -> Result<Option<common::policy::PolicyConfig>> {
    use common::policy::{FilesystemPolicyConfig, NetworkPolicyConfig, PolicyProfile};

    let mut config = match args.policy_profile.as_deref() {
        Some("http_api") => Some(PolicyProfile::HttpApi.to_config()),
        Some("background_worker") => Some(PolicyProfile::BackgroundWorker.to_config()),
        Some("static_site") => Some(PolicyProfile::StaticSite.to_config()),
        Some("database_proxy") => Some(PolicyProfile::DatabaseProxy.to_config()),
        Some("unrestricted") => Some(PolicyProfile::Unrestricted.to_config()),
        Some(other) => {
            anyhow::bail!(
                "Unknown policy profile '{}'. Available: http_api, background_worker, static_site, database_proxy, unrestricted",
                other
            );
        }
        None => None,
    };

    let has_network_overrides = args.policy_network_allow_outbound_tcp.is_some()
        || args.policy_network_allow_outbound_udp.is_some()
        || args.policy_network_allow_dns.is_some()
        || args.policy_network_allowed_cidrs.is_some()
        || args.policy_network_denied_cidrs.is_some()
        || args.policy_network_max_outbound_connections.is_some()
        || args.policy_network_max_egress_bytes.is_some();

    let has_fs_overrides = args.policy_fs_max_open_fds.is_some()
        || args.policy_fs_max_write_bytes.is_some()
        || args.policy_fs_allow_file_create.is_some()
        || args.policy_fs_allow_file_delete.is_some()
        || args.policy_fs_allowed_paths.is_some();

    if has_network_overrides || has_fs_overrides {
        config = Some(config.unwrap_or_default());
        let cfg = config.as_mut().unwrap();

        if has_network_overrides {
            let net = cfg.network.get_or_insert_with(NetworkPolicyConfig::default);
            if let Some(v) = args.policy_network_allow_outbound_tcp {
                net.allow_outbound_tcp = Some(v);
            }
            if let Some(v) = args.policy_network_allow_outbound_udp {
                net.allow_outbound_udp = Some(v);
            }
            if let Some(v) = args.policy_network_allow_dns {
                net.allow_dns = Some(v);
            }
            if let Some(ref cidrs) = args.policy_network_allowed_cidrs {
                net.allowed_cidrs = Some(parse_cidr_list(cidrs)?);
            }
            if let Some(ref cidrs) = args.policy_network_denied_cidrs {
                net.denied_cidrs = Some(parse_cidr_list(cidrs)?);
            }
            if let Some(v) = args.policy_network_max_outbound_connections {
                net.max_outbound_connections = Some(v);
            }
            if let Some(v) = args.policy_network_max_egress_bytes {
                net.max_egress_bytes = Some(v);
            }
        }

        if has_fs_overrides {
            let fs = cfg
                .filesystem
                .get_or_insert_with(FilesystemPolicyConfig::default);
            if let Some(v) = args.policy_fs_max_open_fds {
                fs.max_open_fds = Some(v);
            }
            if let Some(v) = args.policy_fs_max_write_bytes {
                fs.max_fs_write_bytes = Some(v);
            }
            if let Some(v) = args.policy_fs_allow_file_create {
                fs.allow_file_create = Some(v);
            }
            if let Some(v) = args.policy_fs_allow_file_delete {
                fs.allow_file_delete = Some(v);
            }
            if let Some(ref paths) = args.policy_fs_allowed_paths {
                fs.allowed_paths = Some(paths.split(',').map(|s| s.trim().to_string()).collect());
            }
        }
    }

    if let Some(ref cfg) = config {
        if let Some(ref net) = cfg.network {
            if let Some(ref cidrs) = net.allowed_cidrs {
                for cidr in cidrs {
                    if cidr.parse::<ipnet::IpNet>().is_err() {
                        anyhow::bail!("Invalid allowed CIDR: {:?}", cidr);
                    }
                }
            }
            if let Some(ref cidrs) = net.denied_cidrs {
                for cidr in cidrs {
                    if cidr.parse::<ipnet::IpNet>().is_err() {
                        anyhow::bail!("Invalid denied CIDR: {:?}", cidr);
                    }
                }
            }
        }
    }

    Ok(config)
}

fn parse_cidr_list(cidrs: &str) -> Result<Vec<String>> {
    Ok(cidrs
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}
