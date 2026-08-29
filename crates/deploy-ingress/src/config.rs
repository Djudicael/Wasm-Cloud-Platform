use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;

pub const DEFAULT_HA_LEASE_BUCKET: &str = "DEPLOY_INGRESS_HA";
pub const DEFAULT_CREDENTIAL_BUCKET: &str = "DEPLOY_INGRESS_CREDENTIALS";

#[derive(Parser)]
pub struct Args {
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_ENVIRONMENT",
        default_value = "development"
    )]
    pub environment: String,
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

impl Args {
    pub fn validate_security_policy(&self) -> anyhow::Result<()> {
        match self.environment.as_str() {
            "development" | "test" => return Ok(()),
            "production" => {}
            other => anyhow::bail!(
                "invalid environment {other}; expected development, test, or production"
            ),
        }

        if !self.auth_enabled {
            anyhow::bail!("production requires deploy-ingress authentication");
        }
        common::auth::validate_production_bearer_token(
            "deploy-ingress read token",
            self.auth_read_token.as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        common::auth::validate_production_bearer_token(
            "deploy-ingress write token",
            self.auth_write_token.as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        if self.auth_read_token == self.auth_write_token {
            anyhow::bail!("deploy-ingress read and write tokens must be different");
        }
        if self.key_source != "file"
            || self
                .key_file
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            anyhow::bail!(
                "production requires key_source=file backed by a read-only tmpfs projection from the external secret manager"
            );
        }
        if self
            .nats_creds
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
            || !self.nats_url.starts_with("tls://")
        {
            anyhow::bail!("production requires NATS credentials and a tls:// URL");
        }
        if !self.bind_address.parse::<std::net::IpAddr>()?.is_loopback() {
            anyhow::bail!(
                "production deploy ingress must bind to loopback behind the TLS front door"
            );
        }
        if !self
            .advertised_artifact_url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://"))
        {
            anyhow::bail!("production advertised artifact URL must use https");
        }
        if !self.ha_enabled {
            anyhow::bail!("production requires deploy-ingress HA");
        }
        if !self.require_signature || !self.require_oci_digest_refs {
            anyhow::bail!(
                "production requires artifact signatures and digest-pinned OCI references"
            );
        }
        let has_signature_scope = [
            self.allowed_issuers.as_deref(),
            self.allowed_identities.as_deref(),
            self.allowed_repositories.as_deref(),
            self.allowed_namespaces.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty());
        if !has_signature_scope {
            anyhow::bail!("production signature verification requires an explicit allow-list");
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_rejects_local_defaults() {
        let args = Args::try_parse_from(["ingress", "--environment", "production"]).unwrap();
        assert!(args.validate_security_policy().is_err());
    }

    #[test]
    fn production_accepts_external_projection_and_locked_artifacts() {
        let args = Args::try_parse_from([
            "ingress",
            "--environment",
            "production",
            "--auth-enabled",
            "--auth-read-token",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--auth-write-token",
            "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
            "--key-source",
            "file",
            "--key-file",
            "/run/secrets/deploy-ingress-kek",
            "--nats-url",
            "tls://nats.internal:4222",
            "--nats-creds",
            "/run/secrets/nats.creds",
            "--advertised-artifact-url",
            "https://ingress.internal/artifacts",
            "--require-signature",
            "--allowed-repositories",
            "example/platform",
            "--require-oci-digest-refs",
        ])
        .unwrap();
        assert!(args.validate_security_policy().is_ok());
    }
}
