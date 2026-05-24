// crates/ctl/src/cmds/manifest.rs
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Full deployment manifest schema.
/// Single source of truth for deploying an app on the WASI Cloud Platform.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeployManifest {
    #[serde(default)]
    pub app: AppManifestSection,

    #[serde(default)]
    pub fuel: FuelManifestSection,

    #[serde(default)]
    pub policy: Option<common::policy::PolicyConfig>,

    #[serde(default)]
    pub gateway: Option<GatewayManifestSection>,

    #[serde(default)]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub secrets: HashMap<String, SecretRef>,

    #[serde(default)]
    pub api_keys: Vec<common::types::ApiKeyRecord>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppManifestSection {
    pub name: String,
    pub version: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    #[serde(default)]
    pub description: String,
    pub wasm_artifact: String,
    #[serde(default = "default_wasm_bind_port")]
    pub wasm_bind_port: u16,
}

fn default_namespace() -> String {
    "default".to_string()
}

fn default_wasm_bind_port() -> u16 {
    8080
}

impl Default for AppManifestSection {
    fn default() -> Self {
        AppManifestSection {
            name: String::new(),
            version: "v1".to_string(),
            namespace: default_namespace(),
            description: String::new(),
            wasm_artifact: String::new(),
            wasm_bind_port: default_wasm_bind_port(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FuelManifestSection {
    #[serde(default = "default_quota")]
    pub quota: u64,
    #[serde(default = "default_memory_pages")]
    pub memory_pages: u32,
    #[serde(default = "default_max_instances")]
    pub max_instances: u32,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
}

fn default_quota() -> u64 {
    500_000_000
}
fn default_memory_pages() -> u32 {
    2048
}
fn default_max_instances() -> u32 {
    10
}
fn default_idle_timeout() -> u64 {
    300
}

impl Default for FuelManifestSection {
    fn default() -> Self {
        FuelManifestSection {
            quota: default_quota(),
            memory_pages: default_memory_pages(),
            max_instances: default_max_instances(),
            idle_timeout_secs: default_idle_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayManifestSection {
    #[serde(default)]
    pub host: Option<String>,

    #[serde(default)]
    pub auth: Option<GatewayAuthManifest>,

    #[serde(default)]
    pub cors: Option<common::types::CorsPolicy>,

    #[serde(default)]
    pub rate_limit: Option<common::types::RouteRateLimit>,

    #[serde(default)]
    pub circuit_breaker: Option<common::types::CircuitBreakerConfig>,

    #[serde(default)]
    pub transform: Option<common::types::RequestTransform>,

    #[serde(default)]
    pub endpoints: Vec<EndpointRuleManifest>,
}

/// Endpoint rule for manifest parsing (with flattened auth for TOML compatibility).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EndpointRuleManifest {
    pub path: String,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default = "default_auth_policy")]
    pub auth: String,
    #[serde(default)]
    pub allowed_roles: Vec<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<common::types::RouteRateLimit>,
}

fn default_auth_policy() -> String {
    "inherit".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayAuthManifest {
    #[serde(default)]
    pub policy: String,
    #[serde(default)]
    pub allowed_roles: Vec<String>,
    #[serde(default)]
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretRef {
    pub r#ref: String,
}

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
                    rate_limit: e.rate_limit.clone(),
                })
                .collect(),
        })
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_parse_minimal() {
        let toml = r#"
[app]
name = "test-app"
version = "v1"
namespace = "production"
wasm_artifact = "./test.wasm"

[fuel]
quota = 100000000
memory_pages = 512
max_instances = 5
idle_timeout_secs = 60
"#;
        let manifest: DeployManifest = toml::from_str(toml).unwrap();
        assert_eq!(manifest.app.name, "test-app");
        assert_eq!(manifest.app.version, "v1");
        assert_eq!(manifest.app.namespace, "production");
        assert_eq!(manifest.app.wasm_artifact, "./test.wasm");
        assert_eq!(manifest.fuel.quota, 100000000);
        assert_eq!(manifest.fuel.memory_pages, 512);
    }

    #[test]
    fn test_manifest_parse_with_gateway() {
        let toml = r#"
[app]
name = "api-users"
version = "v2"
wasm_artifact = "./api.wasm"

[gateway]
host = "api.example.com"

[gateway.auth]
policy = "roles"
allowed_roles = ["admin", "user"]

[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"

[[gateway.endpoints]]
path = "/api/admin"
methods = ["POST"]
auth = "roles"
allowed_roles = ["admin"]
"#;
        let manifest: DeployManifest = toml::from_str(toml).unwrap();
        let gw = manifest.gateway.as_ref().unwrap();
        assert_eq!(gw.host, Some("api.example.com".to_string()));
        let auth = gw.auth.as_ref().unwrap();
        assert_eq!(auth.policy, "roles");
        assert_eq!(auth.allowed_roles, vec!["admin", "user"]);
        assert_eq!(gw.endpoints.len(), 2);
        assert_eq!(gw.endpoints[0].path, "/health");
        assert_eq!(gw.endpoints[0].auth, "none");
        assert_eq!(gw.endpoints[1].path, "/api/admin");
    }

    #[test]
    fn test_manifest_to_app_config() {
        let manifest = DeployManifest {
            app: AppManifestSection {
                name: "test".to_string(),
                version: "v1".to_string(),
                namespace: "staging".to_string(),
                description: String::new(),
                wasm_artifact: "./test.wasm".to_string(),
                wasm_bind_port: 8080,
            },
            fuel: FuelManifestSection {
                quota: 1000000,
                memory_pages: 1024,
                max_instances: 3,
                idle_timeout_secs: 120,
            },
            policy: None,
            gateway: None,
            env: {
                let mut m = HashMap::new();
                m.insert("LOG_LEVEL".to_string(), "debug".to_string());
                m
            },
            secrets: HashMap::new(),
            api_keys: vec![],
        };

        let config = manifest.to_app_config();
        assert_eq!(config.id.0, "staging/test:v1");
        assert_eq!(config.namespace, "staging");
        assert_eq!(config.fuel_quota.0, 1000000);
        assert_eq!(config.memory_limit.0, 1024);
        assert_eq!(config.max_instances, 3);
        assert_eq!(config.env_vars.get("LOG_LEVEL"), Some(&"debug".to_string()));
    }

    #[test]
    fn test_manifest_to_gateway_config() {
        let manifest = DeployManifest {
            app: AppManifestSection::default(),
            fuel: FuelManifestSection::default(),
            policy: None,
            gateway: Some(GatewayManifestSection {
                host: Some("api.example.com".to_string()),
                auth: Some(GatewayAuthManifest {
                    policy: "authenticated".to_string(),
                    allowed_roles: vec![],
                    client_id: None,
                }),
                cors: None,
                rate_limit: Some(common::types::RouteRateLimit {
                    requests_per_second: 100,
                    burst_capacity: 20,
                    distributed: false,
                }),
                circuit_breaker: None,
                transform: None,
                endpoints: vec![EndpointRuleManifest {
                    path: "/health".to_string(),
                    methods: vec!["GET".to_string()],
                    auth: "none".to_string(),
                    allowed_roles: vec![],
                    client_id: None,
                    rate_limit: None,
                }],
            }),
            env: HashMap::new(),
            secrets: HashMap::new(),
            api_keys: vec![],
        };

        let gw = manifest.to_gateway_config().unwrap();
        assert_eq!(gw.auth, common::types::AuthPolicy::Authenticated);
        assert_eq!(gw.rate_limit.as_ref().unwrap().requests_per_second, 100);
        assert!(!gw.rate_limit.as_ref().unwrap().distributed);
        assert_eq!(gw.endpoints.len(), 1);
    }

    #[test]
    fn test_manifest_rate_limit_defaults_to_node_local_when_distributed_is_omitted() {
        let toml = r#"
[app]
name = "api-users"
version = "v1"
wasm_artifact = "./api.wasm"

[gateway.rate_limit]
requests_per_second = 100
burst_capacity = 20
"#;

        let manifest: DeployManifest = toml::from_str(toml).unwrap();
        let rate_limit = manifest
            .gateway
            .as_ref()
            .and_then(|gateway| gateway.rate_limit.as_ref())
            .unwrap();
        assert!(!rate_limit.distributed);
    }
}
