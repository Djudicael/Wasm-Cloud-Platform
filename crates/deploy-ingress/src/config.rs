use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;

pub const DEFAULT_HA_LEASE_BUCKET: &str = "DEPLOY_INGRESS_HA";
pub const DEFAULT_CREDENTIAL_BUCKET: &str = "DEPLOY_INGRESS_CREDENTIALS";

#[derive(Parser, Debug)]
pub struct Args {
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_ID",
        default_value = "deploy-ingress-0"
    )]
    pub ingress_id: String,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_NATS_URL",
        default_value = "nats://127.0.0.1:4222"
    )]
    pub nats_url: String,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_NATS_CREDS")]
    pub nats_creds: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_DB_PATH",
        default_value = "/tmp/wasm-deploy-ingress/state.redb"
    )]
    pub db_path: PathBuf,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_BIND_ADDRESS",
        default_value = "127.0.0.1"
    )]
    pub bind_address: String,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_PORT", default_value_t = 9092)]
    pub deploy_port: u16,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_ARTIFACT_PORT",
        default_value_t = 9091
    )]
    pub artifact_port: u16,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ADVERTISED_ARTIFACT_URL")]
    pub advertised_artifact_url: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_AUTH_ENABLED",
        default_value_t = false
    )]
    pub auth_enabled: bool,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_AUTH_READ_TOKEN")]
    pub auth_read_token: Option<String>,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_AUTH_WRITE_TOKEN")]
    pub auth_write_token: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_KEY_SOURCE",
        default_value = "generate"
    )]
    pub key_source: String,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_KEY_FILE")]
    pub key_file: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_AUDIT_PATH",
        default_value = "/tmp/wasm-deploy-ingress/audit.jsonl"
    )]
    pub audit_path: PathBuf,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_HA_ENABLED", default_value_t = true)]
    pub ha_enabled: bool,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_HA_LEASE_BUCKET",
        default_value = DEFAULT_HA_LEASE_BUCKET
    )]
    pub ha_lease_bucket: String,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_CREDENTIAL_BUCKET",
        default_value = DEFAULT_CREDENTIAL_BUCKET
    )]
    pub credential_bucket: String,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_HA_LEASE_TTL_SECS",
        default_value_t = 30
    )]
    pub ha_lease_ttl_secs: u64,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_HA_LEASE_REFRESH_SECS",
        default_value_t = 10
    )]
    pub ha_lease_refresh_secs: u64,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_REQUIRE_SIGNATURE",
        default_value_t = false
    )]
    pub require_signature: bool,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ALLOWED_ISSUERS")]
    pub allowed_issuers: Option<String>,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ALLOWED_IDENTITIES")]
    pub allowed_identities: Option<String>,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ALLOWED_REPOSITORIES")]
    pub allowed_repositories: Option<String>,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ALLOWED_NAMESPACES")]
    pub allowed_namespaces: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_REQUIRE_OCI_DIGEST_REFS",
        default_value_t = false
    )]
    pub require_oci_digest_refs: bool,
}

pub fn parse_csv_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn socket_addr(host: &str, port: u16) -> anyhow::Result<std::net::SocketAddr> {
    let ip: std::net::IpAddr = host
        .parse()
        .with_context(|| format!("invalid bind address {host}"))?;
    Ok(std::net::SocketAddr::new(ip, port))
}
