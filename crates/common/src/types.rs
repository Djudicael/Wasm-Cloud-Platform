use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterNodeRecord {
    pub node_id: String,
    pub last_seen_unix_secs: u64,
    pub joined_at_unix_secs: Option<u64>,
    pub health_status: crate::health::NodeHealthStatus,
    pub proxy_address: Option<String>,
    pub artifact_server_url: Option<String>,
    pub protocol_version: Option<u32>,
    pub binary_version: Option<String>,
    pub secret_transport_public_key: Option<String>,
    pub accepting_requests: Option<bool>,
    pub active_instances: Option<u32>,
    pub deployed_apps: Option<u32>,
}

impl ClusterNodeRecord {
    pub fn new(node_id: impl Into<String>, last_seen_unix_secs: u64) -> Self {
        Self {
            node_id: node_id.into(),
            last_seen_unix_secs,
            joined_at_unix_secs: None,
            health_status: crate::health::NodeHealthStatus::Healthy,
            proxy_address: None,
            artifact_server_url: None,
            protocol_version: None,
            binary_version: None,
            secret_transport_public_key: None,
            accepting_requests: None,
            active_instances: None,
            deployed_apps: None,
        }
    }

    pub fn is_stale(&self, max_age_secs: u64) -> bool {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| {
                elapsed.as_secs().saturating_sub(self.last_seen_unix_secs) > max_age_secs
            })
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Route {
    /// The Host header value to match. Supports exact match only for now.
    /// e.g. "api.myapp.com" or "myapp.com"
    pub host: String,

    /// The target app. Must exist in the [configs] table.
    pub app_id: AppId,

    /// Optional path prefix (default "/").
    pub path_prefix: String,

    /// If true, strip the path_prefix before forwarding.
    pub strip_prefix: bool,

    pub created_at: u64,
    pub updated_at: u64,
}
impl AppId {
    pub fn new(name: &str, version: &str) -> Self {
        let id = format!("{name}:{version}");
        Self::new_validate(&id).unwrap_or_else(|_| {
            panic!("Invalid AppId: name and version must be non-empty and contain no whitespace or NATS-invalid characters (> * . newline)")
        })
    }

    /// Create a qualified AppId with namespace.
    /// Format: "<namespace>/<name>:<version>"
    pub fn new_namespaced(namespace: &str, name: &str, version: &str) -> Self {
        let id = format!("{namespace}/{name}:{version}");
        Self::new_validate(&id).unwrap_or_else(|_| {
            panic!("Invalid AppId: namespace, name and version must be non-empty and contain no whitespace or NATS-invalid characters (> * . newline)")
        })
    }

    pub fn new_validate(s: &str) -> Result<Self, &'static str> {
        if s.is_empty() {
            return Err("AppId cannot be empty");
        }
        if s.contains(' ') || s.contains('\n') || s.contains('\t') {
            return Err("AppId cannot contain whitespace");
        }
        if s.contains('>') || s.contains('*') {
            return Err("AppId cannot contain > or * (invalid in NATS subjects)");
        }
        // Allow '/' and ':' for qualified format
        Ok(AppId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Extract namespace from a qualified AppId.
    /// For "default/api-users:v2" returns "default".
    /// For "api-users:v2" returns "default".
    pub fn namespace(&self) -> &str {
        if self.0.contains('/') {
            self.0.split('/').next().unwrap_or("default")
        } else {
            "default"
        }
    }

    /// Extract bare name (without namespace) from a qualified AppId.
    /// For "default/api-users:v2" returns "api-users:v2".
    /// For "api-users:v2" returns "api-users:v2".
    pub fn bare_name(&self) -> &str {
        if self.0.contains('/') {
            self.0.split('/').nth(1).unwrap_or(&self.0)
        } else {
            &self.0
        }
    }

    /// Extract bare app name without version.
    /// For "default/api-users:v2" returns "api-users".
    pub fn bare_app_name(&self) -> &str {
        let name_part = self.bare_name();
        name_part.split(':').next().unwrap_or(name_part)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(pub Uuid);
impl InstanceId {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        InstanceId(Uuid::new_v4())
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct FuelQuota(pub u64);
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryPages(pub u32);
impl MemoryPages {
    pub fn to_bytes(self) -> u64 {
        self.0 as u64 * 65536
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ExtendedLimits {
    pub max_open_fds: u32,
    pub max_fs_write_bytes: u64,
    pub max_net_egress_bytes: u64,
    pub max_outbound_connections: u32,
    pub max_table_elements: u32,
}

impl Default for ExtendedLimits {
    fn default() -> Self {
        ExtendedLimits {
            max_open_fds: 64,
            max_fs_write_bytes: 50 * 1024 * 1024,
            max_net_egress_bytes: 10 * 1024 * 1024,
            max_outbound_connections: 16,
            max_table_elements: 10_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtendedLimitsConfig {
    pub max_open_fds: Option<u32>,
    pub max_fs_write_bytes: Option<u64>,
    pub max_net_egress_bytes: Option<u64>,
    pub max_outbound_connections: Option<u32>,
    pub max_table_elements: Option<u32>,
}

impl ExtendedLimitsConfig {
    pub fn to_limits(&self) -> ExtendedLimits {
        let defaults = ExtendedLimits::default();
        ExtendedLimits {
            max_open_fds: self.max_open_fds.unwrap_or(defaults.max_open_fds),
            max_fs_write_bytes: self
                .max_fs_write_bytes
                .unwrap_or(defaults.max_fs_write_bytes),
            max_net_egress_bytes: self
                .max_net_egress_bytes
                .unwrap_or(defaults.max_net_egress_bytes),
            max_outbound_connections: self
                .max_outbound_connections
                .unwrap_or(defaults.max_outbound_connections),
            max_table_elements: self
                .max_table_elements
                .unwrap_or(defaults.max_table_elements),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Unique app identifier: "<name>:<version>" or "<namespace>/<name>:<version>"
    pub id: AppId,

    /// Maximum Fuel units per execution.
    pub fuel_quota: FuelQuota,

    /// Maximum linear memory pages (1 page = 64 KiB).
    pub memory_limit: MemoryPages,

    /// Maximum concurrent instances for this app on this node.
    pub max_instances: u32,

    /// Idle timeout: kill instance if no requests for this many seconds.
    pub idle_timeout_secs: u64,

    /// Port the Wasm app binds internally (usually 8080).
    pub wasm_bind_port: u16,

    /// Static environment variables (non-secret).
    /// Secrets are stored separately in the [secrets] table.
    pub env_vars: std::collections::HashMap<String, String>,

    /// List of secret keys to inject (resolved from the secrets table).
    /// e.g. ["DATABASE_URL", "STRIPE_KEY"]
    pub secret_keys: Vec<String>,

    #[serde(default)]
    pub extended_limits: Option<ExtendedLimitsConfig>,

    pub health_check_path: Option<String>,

    /// Maximum simultaneous database connections this app is allowed to hold.
    /// This is used for documentation/audit purposes. The actual enforcement
    /// is done by pgBouncer via max_client_conn and per-user limits.
    #[serde(default)]
    pub db_max_connections: Option<u32>,

    /// Rate limit configuration for this app.
    #[serde(default)]
    pub rate_limit: Option<AppRateLimitConfig>,

    /// Tenant identifier for billing attribution.
    /// If not specified, the app name (without version) is used as the tenant.
    #[serde(default)]
    pub tenant_id: Option<String>,

    #[serde(default)]
    pub policy: Option<crate::policy::PolicyConfig>,

    /// The namespace this app belongs to.
    /// Namespaces are flat strings (e.g., "production", "tenant-a").
    /// Default = "default". The Wasm app never sees this value.
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

fn default_namespace() -> String {
    "default".to_string()
}

/// Rate limit configuration, stored as part of AppConfig.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppRateLimitConfig {
    /// Maximum sustained requests per second per node.
    pub requests_per_second: u32,

    /// Burst tolerance (number of requests above the sustained rate).
    pub burst_capacity: u32,

    /// Maximum requests per second from a single IP to this app.
    pub per_ip_limit: u32,
}

impl Default for AppRateLimitConfig {
    fn default() -> Self {
        AppRateLimitConfig {
            requests_per_second: 1_000,
            burst_capacity: 50,
            per_ip_limit: 100,
        }
    }
}

impl AppConfig {
    /// Default safe config for a new app.
    pub fn default_for(app_id: AppId) -> Self {
        AppConfig {
            id: app_id,
            fuel_quota: FuelQuota(500_000_000), // ~500ms of compute
            memory_limit: MemoryPages(2048),    // 128 MB
            max_instances: 10,
            idle_timeout_secs: 300,
            wasm_bind_port: 8080,
            env_vars: std::collections::HashMap::new(),
            secret_keys: Vec::new(),
            extended_limits: None,
            health_check_path: None,
            db_max_connections: None,
            rate_limit: None,
            tenant_id: None,
            policy: None,
            namespace: default_namespace(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InstanceState {
    Starting,
    Ready {
        addr: std::net::SocketAddr,
    },
    Busy,
    /// Removed from routing and discovery; waiting for in-flight work to drain.
    Draining {
        addr: std::net::SocketAddr,
    },
    /// Shutdown signal has been sent and the supervisor is waiting for process exit.
    Stopping {
        addr: std::net::SocketAddr,
    },
    /// Grace timeout elapsed; the worker is fenced but not yet confirmed exited.
    ExitTimedOut {
        addr: std::net::SocketAddr,
    },
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsConfig {
    pub platform_domain: Option<String>,
    pub dns_webhook_url: Option<String>,
    pub dns_webhook_token: Option<String>,
    pub admin_port: u16,
}

impl Default for DnsConfig {
    fn default() -> Self {
        DnsConfig {
            platform_domain: None,
            dns_webhook_url: None,
            dns_webhook_token: None,
            admin_port: 9053,
        }
    }
}

impl DnsConfig {
    pub fn default_with_port(port: u16) -> Self {
        DnsConfig {
            platform_domain: None,
            dns_webhook_url: None,
            dns_webhook_token: None,
            admin_port: port,
        }
    }
}

// ── Gateway Configuration ────────────────────────────────────────────────────

/// Gateway configuration for a single route.
/// Stored per-route in the route table. All fields are optional —
/// a route with no gateway config is fully public (no auth, no CORS,
/// default rate limits).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GatewayRouteConfig {
    /// Authentication policy for this route.
    #[serde(default)]
    pub auth: AuthPolicy,

    /// CORS policy. None = no CORS headers (browsers will block cross-origin).
    #[serde(default)]
    pub cors: Option<CorsPolicy>,

    /// Request transformation rules.
    #[serde(default)]
    pub transform: Option<RequestTransform>,

    /// Override the app-level rate limit for this route.
    #[serde(default)]
    pub rate_limit: Option<RouteRateLimit>,

    /// Circuit breaker configuration for this route's upstream.
    #[serde(default)]
    pub circuit_breaker: Option<CircuitBreakerConfig>,

    /// Per-endpoint security overrides.
    /// Evaluated top-to-bottom; first match wins.
    #[serde(default)]
    pub endpoints: Vec<EndpointRule>,
}

/// Authentication policy for a route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthPolicy {
    /// No authentication required. Anyone can access this route.
    #[default]
    None,

    /// Valid JWT required. The token is validated against the OIDC provider's
    /// JWKS endpoint. User identity is extracted and forwarded to the upstream
    /// via X-User-Id, X-User-Roles headers.
    Authenticated,

    /// Valid JWT required AND the user must have at least one of the specified
    /// roles. Roles are checked against the JWT's `realm_access.roles` or
    /// `resource_access.<client_id>.roles` claims.
    Roles {
        /// Which roles are accepted (OR logic — any match passes).
        allowed_roles: Vec<String>,
        /// Which Keycloak client's roles to check.
        /// None = check realm_access.roles.
        /// Some("my-client") = check resource_access.my-client.roles.
        client_id: Option<String>,
    },
}

/// CORS policy for a route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorsPolicy {
    /// Allowed origins. Use "*" for public APIs, or list specific origins.
    pub allowed_origins: Vec<String>,

    /// Allowed HTTP methods.
    #[serde(default = "CorsPolicy::default_methods")]
    pub allowed_methods: Vec<String>,

    /// Allowed request headers.
    #[serde(default = "CorsPolicy::default_headers")]
    pub allowed_headers: Vec<String>,

    /// Headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,

    /// Whether to include credentials in cross-origin requests.
    #[serde(default)]
    pub allow_credentials: bool,

    /// How long the browser can cache the preflight response (seconds).
    #[serde(default = "CorsPolicy::default_max_age")]
    pub max_age_secs: u32,
}

impl CorsPolicy {
    pub fn default_methods() -> Vec<String> {
        vec![
            "GET".into(),
            "POST".into(),
            "PUT".into(),
            "DELETE".into(),
            "PATCH".into(),
            "OPTIONS".into(),
        ]
    }
    pub fn default_headers() -> Vec<String> {
        vec![
            "Authorization".into(),
            "Content-Type".into(),
            "X-Request-Id".into(),
        ]
    }
    fn default_max_age() -> u32 {
        86400
    }
}

/// Request transformation rules applied before forwarding to upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RequestTransform {
    /// Headers to add to the request before forwarding.
    pub add_headers: Vec<(String, String)>,

    /// Headers to remove from the request before forwarding.
    pub remove_headers: Vec<String>,

    /// Path prefix to add before the existing path.
    pub path_prefix: Option<String>,

    /// Query parameters to strip from the request before forwarding.
    pub strip_query_params: Vec<String>,
}

/// Per-route rate limit override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteRateLimit {
    /// Requests per second for this specific route (overrides app-level).
    pub requests_per_second: u32,

    /// Burst capacity for this route.
    pub burst_capacity: u32,

    /// Whether this route's rate limit is shared across all nodes (distributed)
    /// or node-local only. Default: node-local.
    #[serde(default)]
    pub distributed: bool,
}

/// Circuit breaker configuration per upstream app.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before opening the circuit.
    #[serde(default = "CircuitBreakerConfig::default_failure_threshold")]
    pub failure_threshold: u32,

    /// Duration in seconds to keep the circuit open before allowing a
    /// half-open probe request through.
    #[serde(default = "CircuitBreakerConfig::default_reset_timeout_secs")]
    pub reset_timeout_secs: u32,

    /// What counts as a "failure". Default: 5xx responses and connection errors.
    #[serde(default)]
    pub failure_criteria: FailureCriteria,
}

impl CircuitBreakerConfig {
    fn default_failure_threshold() -> u32 {
        5
    }
    fn default_reset_timeout_secs() -> u32 {
        30
    }
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        CircuitBreakerConfig {
            failure_threshold: Self::default_failure_threshold(),
            reset_timeout_secs: Self::default_reset_timeout_secs(),
            failure_criteria: FailureCriteria::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FailureCriteria {
    /// Default: 5xx responses and connection errors.
    #[default]
    ServerErrors,
    /// 5xx responses, connection errors, and 4xx responses.
    AllErrors,
    /// Custom: count only specific HTTP status codes as failures.
    StatusCodes(Vec<u16>),
}

/// Per-endpoint security rule for fine-grained gateway policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndpointRule {
    /// Path prefix to match. Exact match for now.
    pub path: String,

    /// HTTP methods this rule applies to. Empty = all methods.
    #[serde(default)]
    pub methods: Vec<String>,

    /// Authentication policy for this endpoint.
    #[serde(default)]
    pub auth: EndpointAuth,

    /// Optional rate limit override.
    pub rate_limit: Option<RouteRateLimit>,
}

/// Authentication methods supported at the endpoint level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EndpointAuth {
    /// Inherit from route-level config.
    #[default]
    Inherit,

    /// No authentication required.
    None,

    /// Valid JWT required.
    Authenticated,

    /// Valid JWT + one of the specified roles.
    Roles {
        allowed_roles: Vec<String>,
        client_id: Option<String>,
    },

    /// API key authentication via X-Api-Key header.
    ApiKey,
}

/// API key record stored in redb.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKeyRecord {
    pub name: String,
    /// "sha256$<hex>" hashed key
    pub key_hash: String,
    /// Allowed path prefixes
    pub scopes: Vec<String>,
}

/// OIDC provider configuration. Stored once per platform (not per-route).
/// All routes that require auth share the same OIDC provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OidcConfig {
    /// The OIDC issuer URL (e.g., "https://keycloak.example.com/realms/my-realm")
    /// Used to fetch /.well-known/openid-configuration
    pub issuer_url: String,

    /// Expected audience (aud claim) in the JWT.
    /// Must match the client_id configured in Keycloak.
    pub audience: String,

    /// How often to refresh the JWKS cache (seconds).
    /// Default: 3600 (1 hour). Keycloak rotates keys infrequently.
    #[serde(default = "default_jwks_refresh_secs")]
    pub jwks_refresh_secs: u64,

    /// Clock skew tolerance (seconds). Allows JWTs that are slightly
    /// expired due to clock differences between nodes.
    #[serde(default = "default_clock_skew_secs")]
    pub clock_skew_secs: u64,
}

fn default_jwks_refresh_secs() -> u64 {
    3600
}
fn default_clock_skew_secs() -> u64 {
    30
}
