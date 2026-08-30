use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct DeployArgs {
    /// Application name (e.g. "api-users")
    #[arg(long)]
    pub app: Option<String>,

    /// Version string (e.g. "v2")
    #[arg(long)]
    pub version: Option<String>,

    /// Namespace (default = "default")
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// Path to the .wasm binary
    #[arg(long)]
    pub wasm: Option<String>,

    /// Remote HTTP(S) artifact URL for deploy-time ingest
    #[arg(long)]
    pub artifact_url: Option<String>,

    /// Digest-pinned OCI artifact reference (for example oci://ghcr.io/org/app@sha256:...)
    #[arg(long)]
    pub artifact_ref: Option<String>,

    /// Expected SHA-256 for a remote artifact
    #[arg(long)]
    pub sha256: Option<String>,

    /// Platform secret reference for remote artifact fetch credentials
    #[arg(long)]
    pub artifact_credential: Option<String>,

    /// Base64-encoded Ed25519 public key for artifact signature verification
    #[arg(long)]
    pub artifact_public_key: Option<String>,

    /// Signature verification algorithm: ed25519 or cosign-ed25519
    #[arg(long, default_value = "ed25519")]
    pub artifact_signature_algorithm: String,

    /// Base64-encoded Ed25519 signature over the artifact claims payload
    #[arg(long)]
    pub artifact_signature: Option<String>,

    /// Optional signature payload. Required for cosign-ed25519 mode.
    #[arg(long)]
    pub artifact_signature_payload: Option<String>,

    /// Optional signature issuer claim
    #[arg(long)]
    pub artifact_issuer: Option<String>,

    /// Optional signature identity claim
    #[arg(long)]
    pub artifact_identity: Option<String>,

    /// Optional signature repository claim
    #[arg(long)]
    pub artifact_repository: Option<String>,

    /// Optional signature namespace claim
    #[arg(long)]
    pub artifact_namespace: Option<String>,

    /// Path to a deployment manifest TOML file
    #[arg(long)]
    pub manifest: Option<String>,

    /// Fuel quota (CPU units per request)
    #[arg(long, default_value = "500000000")]
    pub fuel: u64,

    /// Memory limit in MB
    #[arg(long, default_value = "128")]
    pub memory_mb: u32,

    /// Max concurrent instances on this node
    #[arg(long, default_value = "10")]
    pub max_instances: u32,

    /// Idle timeout in seconds
    #[arg(long, default_value = "300")]
    pub idle_timeout: u64,

    /// Environment variables (KEY=VALUE, repeatable)
    #[arg(long = "env", value_parser = parse_env_var)]
    pub env_vars: Vec<(String, String)>,

    /// Secret keys to inject (names only, not values)
    #[arg(long = "secret")]
    pub secret_keys: Vec<String>,

    /// Node API URL to upload the artifact to (overrides global --node-api)
    #[arg(long)]
    pub node_api: Option<String>,

    /// Artifact API URL when it is served separately from the admin API.
    #[arg(long)]
    pub artifact_api: Option<String>,

    /// Deploy ingress API URL for remote deploy intent (overrides global --deploy-api)
    #[arg(long)]
    pub deploy_api: Option<String>,

    /// Apply a pre-defined policy profile (http_api, background_worker, static_site, database_proxy, unrestricted)
    #[arg(long)]
    pub policy_profile: Option<String>,

    /// Allow outbound TCP (overrides profile)
    #[arg(long)]
    pub policy_network_allow_outbound_tcp: Option<bool>,

    /// Allow outbound UDP (overrides profile)
    #[arg(long)]
    pub policy_network_allow_outbound_udp: Option<bool>,

    /// Allow DNS lookups (overrides profile)
    #[arg(long)]
    pub policy_network_allow_dns: Option<bool>,

    /// Comma-separated allowed CIDRs (e.g. "10.0.0.0/8,172.16.0.0/12")
    #[arg(long)]
    pub policy_network_allowed_cidrs: Option<String>,

    /// Comma-separated denied CIDRs (e.g. "169.254.169.254/32")
    #[arg(long)]
    pub policy_network_denied_cidrs: Option<String>,

    /// Max concurrent outbound connections
    #[arg(long)]
    pub policy_network_max_outbound_connections: Option<u32>,

    /// Max egress bytes (0 = unlimited)
    #[arg(long)]
    pub policy_network_max_egress_bytes: Option<u64>,

    /// Max open file descriptors
    #[arg(long)]
    pub policy_fs_max_open_fds: Option<u32>,

    /// Max filesystem write bytes (0 = unlimited)
    #[arg(long)]
    pub policy_fs_max_write_bytes: Option<u64>,

    /// Allow file creation
    #[arg(long)]
    pub policy_fs_allow_file_create: Option<bool>,

    /// Allow file deletion
    #[arg(long)]
    pub policy_fs_allow_file_delete: Option<bool>,

    /// Comma-separated allowed filesystem paths
    #[arg(long)]
    pub policy_fs_allowed_paths: Option<String>,

    /// Gateway auth policy: none, authenticated, roles
    #[arg(long)]
    pub gateway_auth: Option<String>,

    /// Comma-separated roles for gateway auth (when policy=roles)
    #[arg(long, value_delimiter = ',')]
    pub gateway_roles: Vec<String>,

    /// Keycloak client ID for role checking
    #[arg(long)]
    pub gateway_oidc_client: Option<String>,

    /// Comma-separated allowed CORS origins
    #[arg(long, value_delimiter = ',')]
    pub gateway_cors_origins: Vec<String>,

    /// Allow credentials in CORS
    #[arg(long)]
    pub gateway_cors_credentials: bool,

    /// Gateway rate limit: requests per second
    #[arg(long)]
    pub gateway_rps: Option<u32>,

    /// Gateway rate limit burst capacity
    #[arg(long)]
    pub gateway_rps_burst: Option<u32>,

    /// Make gateway rate limit distributed across nodes
    #[arg(long)]
    pub gateway_rps_distributed: bool,
}

fn parse_env_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got: {s}"))
}
