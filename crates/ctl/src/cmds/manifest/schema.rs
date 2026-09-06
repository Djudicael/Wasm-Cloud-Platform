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

    #[serde(default)]
    pub artifact: Option<ArtifactManifestSection>,

    #[serde(default)]
    pub placement: PlacementManifestSection,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PlacementManifestSection {
    #[serde(default)]
    pub policy: common::types::PlacementPolicy,
    /// Fully-qualified application IDs, for example
    /// `production/postgres-client:v1`.
    #[serde(default)]
    pub local_dependencies: Vec<common::types::AppId>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppManifestSection {
    pub name: String,
    pub version: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub wasm_artifact: String,
    #[serde(default = "default_wasm_bind_port")]
    pub wasm_bind_port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtifactManifestSection {
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub credential_ref: Option<String>,
    #[serde(default)]
    pub signature: Option<common::deploy::ArtifactSignature>,
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
    pub routes: Vec<RouteManifestSection>,

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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RouteManifestSection {
    pub host: String,
    #[serde(default = "default_path_prefix")]
    pub path_prefix: String,
    #[serde(default)]
    pub strip_prefix: bool,
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
    pub required_scopes: Vec<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<common::types::RouteRateLimit>,
}

fn default_auth_policy() -> String {
    "inherit".to_string()
}

fn default_path_prefix() -> String {
    "/".to_string()
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
