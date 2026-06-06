use super::schema::*;
use anyhow::Result;
use std::collections::HashMap;
use std::collections::HashSet;

impl DeployManifest {
    pub fn from_toml(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read manifest {}: {}", path, e))?;
        let manifest: DeployManifest = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Invalid manifest TOML: {}", e))?;
        Ok(manifest)
    }

    /// Build an AppConfig from this manifest.
    pub fn to_app_config(&self) -> common::types::AppConfig {
        let app_id = common::types::AppId::new_namespaced(
            &self.app.namespace,
            &self.app.name,
            &self.app.version,
        );

        common::types::AppConfig {
            id: app_id,
            fuel_quota: common::types::FuelQuota(self.fuel.quota),
            memory_limit: common::types::MemoryPages(self.fuel.memory_pages),
            max_instances: self.fuel.max_instances,
            idle_timeout_secs: self.fuel.idle_timeout_secs,
            wasm_bind_port: self.app.wasm_bind_port,
            env_vars: self.env.clone(),
            secret_keys: self.secrets.keys().cloned().collect(),
            extended_limits: None,
            health_check_path: None,
            db_max_connections: None,
            rate_limit: None,
            tenant_id: None,
            policy: self.policy.clone(),
            namespace: self.app.namespace.clone(),
        }
    }

    /// Build a GatewayRouteConfig from this manifest's gateway section.
    pub fn to_gateway_config(&self) -> Option<common::types::GatewayRouteConfig> {
        let gw = self.gateway.as_ref()?;
        Some(common::types::GatewayRouteConfig {
            auth: gw
                .auth
                .as_ref()
                .map(|a| match a.policy.as_str() {
                    "none" => common::types::AuthPolicy::None,
                    "authenticated" => common::types::AuthPolicy::Authenticated,
                    "roles" => common::types::AuthPolicy::Roles {
                        allowed_roles: a.allowed_roles.clone(),
                        client_id: a.client_id.clone(),
                    },
                    _ => common::types::AuthPolicy::None,
                })
                .unwrap_or_default(),
            cors: gw.cors.clone(),
            transform: gw.transform.clone(),
            rate_limit: gw.rate_limit.clone(),
            circuit_breaker: gw.circuit_breaker.clone(),
            endpoints: gw
                .endpoints
                .iter()
                .map(|e| common::types::EndpointRule {
                    path: e.path.clone(),
                    methods: e.methods.clone(),
                    auth: match e.auth.as_str() {
                        "none" => common::types::EndpointAuth::None,
                        "authenticated" => common::types::EndpointAuth::Authenticated,
                        "roles" => common::types::EndpointAuth::Roles {
                            allowed_roles: e.allowed_roles.clone(),
                            client_id: e.client_id.clone(),
                        },
                        "api_key" => common::types::EndpointAuth::ApiKey,
                        _ => common::types::EndpointAuth::Inherit,
                    },
                    required_scopes: e.required_scopes.clone(),
                    rate_limit: e.rate_limit.clone(),
                })
                .collect(),
        })
    }

    /// Build public route bindings from the manifest gateway section.
    pub fn to_routes(&self, app_id: &common::types::AppId) -> Result<Vec<common::types::Route>> {
        let Some(gw) = self.gateway.as_ref() else {
            return Ok(Vec::new());
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let mut declared_routes = Vec::new();
        if let Some(host) = gw.host.as_ref() {
            declared_routes.push(RouteManifestSection {
                host: host.clone(),
                path_prefix: "/".to_string(),
                strip_prefix: false,
            });
        }
        declared_routes.extend(gw.routes.iter().cloned());

        let mut seen = HashSet::new();
        let mut routes = Vec::with_capacity(declared_routes.len());
        for route in declared_routes {
            let host = route.host.trim().to_ascii_lowercase();
            if host.is_empty() {
                anyhow::bail!("gateway route host cannot be empty");
            }

            let path_prefix = normalize_path_prefix(&route.path_prefix);
            let route_key = format!("{host}|{path_prefix}");
            if !seen.insert(route_key) {
                anyhow::bail!(
                    "duplicate gateway route for host '{}' and path prefix '{}'",
                    host,
                    path_prefix
                );
            }

            routes.push(common::types::Route {
                host,
                app_id: app_id.clone(),
                path_prefix,
                strip_prefix: route.strip_prefix,
                created_at: now,
                updated_at: now,
            });
        }

        Ok(routes)
    }
}

fn normalize_path_prefix(path_prefix: &str) -> String {
    let trimmed = path_prefix.trim();
    if trimmed.is_empty() || trimmed == "/" {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

/// Reconstruct a manifest from an AppConfig and gateway config (for `app manifest` command).
#[allow(dead_code)]
pub fn manifest_from_config(
    app_config: &common::types::AppConfig,
    gateway_config: Option<&common::types::GatewayRouteConfig>,
    api_keys: &[common::types::ApiKeyRecord],
) -> DeployManifest {
    let (name, version) = if app_config.id.0.contains('/') {
        let parts: Vec<&str> = app_config.id.0.split('/').collect();
        let nv: Vec<&str> = parts[1].split(':').collect();
        (nv[0].to_string(), nv.get(1).unwrap_or(&"v1").to_string())
    } else {
        let parts: Vec<&str> = app_config.id.0.split(':').collect();
        (
            parts[0].to_string(),
            parts.get(1).unwrap_or(&"v1").to_string(),
        )
    };

    let gateway = gateway_config.map(|cfg| GatewayManifestSection {
        host: None,
        routes: Vec::new(),
        auth: match &cfg.auth {
            common::types::AuthPolicy::None => None,
            common::types::AuthPolicy::Authenticated => Some(GatewayAuthManifest {
                policy: "authenticated".to_string(),
                allowed_roles: vec![],
                client_id: None,
            }),
            common::types::AuthPolicy::Roles {
                allowed_roles,
                client_id,
            } => Some(GatewayAuthManifest {
                policy: "roles".to_string(),
                allowed_roles: allowed_roles.clone(),
                client_id: client_id.clone(),
            }),
        },
        cors: cfg.cors.clone(),
        rate_limit: cfg.rate_limit.clone(),
        circuit_breaker: cfg.circuit_breaker.clone(),
        transform: cfg.transform.clone(),
        endpoints: cfg
            .endpoints
            .iter()
            .map(|e| EndpointRuleManifest {
                path: e.path.clone(),
                methods: e.methods.clone(),
                auth: match &e.auth {
                    common::types::EndpointAuth::Inherit => "inherit".to_string(),
                    common::types::EndpointAuth::None => "none".to_string(),
                    common::types::EndpointAuth::Authenticated => "authenticated".to_string(),
                    common::types::EndpointAuth::Roles { .. } => "roles".to_string(),
                    common::types::EndpointAuth::ApiKey => "api_key".to_string(),
                },
                allowed_roles: match &e.auth {
                    common::types::EndpointAuth::Roles { allowed_roles, .. } => {
                        allowed_roles.clone()
                    }
                    _ => vec![],
                },
                required_scopes: e.required_scopes.clone(),
                client_id: match &e.auth {
                    common::types::EndpointAuth::Roles { client_id, .. } => client_id.clone(),
                    _ => None,
                },
                rate_limit: e.rate_limit.clone(),
            })
            .collect(),
    });

    DeployManifest {
        app: AppManifestSection {
            name,
            version,
            namespace: app_config.namespace.clone(),
            description: String::new(),
            wasm_artifact: String::new(),
            wasm_bind_port: app_config.wasm_bind_port,
        },
        fuel: FuelManifestSection {
            quota: app_config.fuel_quota.0,
            memory_pages: app_config.memory_limit.0,
            max_instances: app_config.max_instances,
            idle_timeout_secs: app_config.idle_timeout_secs,
        },
        policy: app_config.policy.clone(),
        gateway,
        env: app_config.env_vars.clone(),
        secrets: HashMap::new(),
        api_keys: api_keys.to_vec(),
        artifact: None,
    }
}
