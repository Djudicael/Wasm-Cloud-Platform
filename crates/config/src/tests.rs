use super::*;
use crate::hot::{merge_hot_config_update, HotConfig, HotConfigUpdate};
use crate::loader::{apply_cli_overrides, apply_env_overrides, merge_config};
use crate::validation::validate_config;
use crate::validation::validate_hot_config;
use common::config::NodeConfig;
use common::config::{DeploymentEnvironment, StorageIntegrityFailureMode, StorageOpenFailureMode};
use common::error::PlatformError;

fn validate_hot(config: &HotConfig) -> Result<(), PlatformError> {
    validate_hot_config(config)
}

#[test]
fn test_default_config_valid() {
    let config = NodeConfig::default();
    assert!(validate_config(&config).is_ok());
}

#[test]
fn production_rejects_local_secret_defaults() {
    let mut config = NodeConfig::default();
    config.node.environment = DeploymentEnvironment::Production;
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("auth.enabled"));
    assert!(error.contains("runtime.key_source"));
    assert!(error.contains("nats.creds_file"));
    assert!(error.contains("tls:// NATS"));
}

#[test]
fn production_accepts_non_exportable_external_key_policy() {
    let mut config = NodeConfig::default();
    config.node.environment = DeploymentEnvironment::Production;
    config.auth.enabled = true;
    config.auth.read_token =
        Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string());
    config.auth.write_token =
        Some("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".to_string());
    config.admin.tls_cert = Some("/run/secrets/admin.crt".to_string());
    config.admin.tls_key = Some("/run/secrets/admin.key".to_string());
    config.nats.url = "tls://nats.internal:4222".to_string();
    config.nats.creds_file = Some("/run/secrets/nats.creds".to_string());
    config.runtime.key_source = "vault-transit".to_string();
    config.runtime.key_vault_url = Some("https://vault.internal".to_string());
    config.runtime.key_vault_token_env = Some("VAULT_TOKEN".to_string());
    config.runtime.key_vault_transit_key = Some("wasm-node-seal".to_string());
    config.runtime.key_vault_transit_context = Some("node-0".to_string());
    config.runtime.key_vault_transit_key_version = Some(1);
    assert!(validate_config(&config).is_ok());
}

#[test]
fn production_rejects_exportable_or_insecure_secret_sources() {
    let mut config = NodeConfig::default();
    config.node.environment = DeploymentEnvironment::Production;
    config.auth.enabled = true;
    config.auth.read_token = Some("a".repeat(64));
    config.auth.write_token = Some("not-a-production-token".to_string());
    config.admin.tls_cert = Some("cert".to_string());
    config.admin.tls_key = Some("key".to_string());
    config.nats.url = "nats://nats.internal:4222".to_string();
    config.nats.creds_file = Some("creds".to_string());
    config.runtime.key_source = "file".to_string();
    config.runtime.key_file = Some("/run/secrets/exported-key".to_string());
    config.dns.webhook_token = Some("inline-secret".to_string());
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("non-exportable"));
    assert!(error.contains("64 hexadecimal"));
    assert!(error.contains("repeated-character"));
    assert!(error.contains("tls:// NATS"));
    assert!(error.contains("inline dns.webhook_token"));
}

#[test]
fn test_toml_parse_minimal() {
    let toml_str = r#"
[node]
node_id = "dev-node"

[nats]
url = "nats://127.0.0.1:4222"

[logging]
level = "debug"
"#;
    let config: NodeConfig = toml::from_str(toml_str).expect("failed to parse minimal TOML");
    assert_eq!(config.node.node_id, "dev-node");
    assert_eq!(config.nats.url, "nats://127.0.0.1:4222");
    assert_eq!(config.logging.level, "debug");
    assert_eq!(config.proxy.http_port, 8080);
    assert_eq!(config.admin.port, 9090);
    assert_eq!(config.admin.bind_address, "127.0.0.1");
    assert_eq!(config.admin.artifact_bind_address, "127.0.0.1");
    assert!(config.admin.advertised_host.is_none());
    assert!(config.admin.advertised_artifact_url.is_none());
}

#[test]
fn test_toml_parse_full() {
    let toml_str = r#"
[node]
node_id = "prod-node-1"

[storage]
db_path = "/var/lib/wasm-node/state.redb"
open_failure_mode = "quarantine_and_recreate"
integrity_failure_mode = "delete_and_exit"

[nats]
url = "nats://nats.prod:4222"
creds_file = "/etc/wasm-node/nats.creds"

[proxy]
http_port = 80
https_port = 443
tls_cert = "/etc/wasm-node/tls/server.crt"
tls_key = "/etc/wasm-node/tls/server.key"

[admin]
port = 9090
artifact_port = 9091
bind_address = "127.0.0.1"
artifact_bind_address = "127.0.0.1"
tls_cert = "/etc/wasm-node/admin/server.crt"
tls_key = "/etc/wasm-node/admin/server.key"
advertised_host = "node-1.internal"
advertised_artifact_url = "https://artifacts.node-1.internal"
auth_token = "secret-token"

[runtime]
port_start = 10000
port_end = 19999
key_source = "file"
key_file = "/etc/wasm-node/master.key"
cache_directory = "/var/cache/wasm-node/wasmtime"
pooling_allocator = true
pooling_total_component_instances = 128
pooling_max_core_instances_per_component = 16
pooling_max_memories_per_component = 8
pooling_max_tables_per_component = 8

[database]
default_url = "postgres://db.prod:5432"
pgbouncer_addr = "127.0.0.1:5432"
enable_db_proxy = true
db_proxy_addr = "127.0.0.1:5433"
db_proxy_backend = "db.internal:5432"
db_proxy_max_connections = 50

[logging]
level = "warn"
otlp_endpoint = "http://collector:4317"

[billing]
export_dir = "/var/lib/wasm-node/billing"
export_interval_secs = 1800

[gc]
artifact_keep_versions = 5
metrics_retain_days = 14
undeploy_grace_secs = 7200
gc_interval_secs = 300
disk_warning_threshold = 0.85

[rate_limit]
default_requests_per_second = 5000
default_burst_capacity = 1000
default_per_ip_limit = 500

[ebpf]
enabled = true
fd_soft_limit = 8192
fd_hard_limit = 9728
mem_low_threshold_pages = 65536
mem_critical_threshold_pages = 16384
disk_slow_threshold_ns = 50000000
tcp_conn_limit_per_pid = 10000
syscall_rate_limit = 100000
sampling_period_secs = 10

[dns]
platform_domain = "myplatform.com"
webhook_url = "https://dns-api.example.com/records"
webhook_token = "secret"

[health]
check_interval_secs = 10
default_idle_timeout_secs = 600
default_max_instances = 20
default_fuel_quota = 1000000000
default_memory_pages = 4096
"#;
    let config: NodeConfig = toml::from_str(toml_str).expect("failed to parse full TOML");
    assert_eq!(config.node.node_id, "prod-node-1");
    assert_eq!(
        config.storage.open_failure_mode,
        StorageOpenFailureMode::QuarantineAndRecreate
    );
    assert_eq!(
        config.storage.integrity_failure_mode,
        StorageIntegrityFailureMode::DeleteAndExit
    );
    assert_eq!(config.proxy.http_port, 80);
    assert_eq!(config.proxy.https_port, 443);
    assert_eq!(
        config.proxy.tls_cert,
        Some("/etc/wasm-node/tls/server.crt".to_string())
    );
    assert_eq!(config.admin.bind_address, "127.0.0.1");
    assert_eq!(config.admin.artifact_bind_address, "127.0.0.1");
    assert_eq!(
        config.admin.tls_cert.as_deref(),
        Some("/etc/wasm-node/admin/server.crt")
    );
    assert_eq!(
        config.admin.tls_key.as_deref(),
        Some("/etc/wasm-node/admin/server.key")
    );
    assert_eq!(
        config.admin.advertised_host.as_deref(),
        Some("node-1.internal")
    );
    assert_eq!(
        config.admin.advertised_artifact_url.as_deref(),
        Some("https://artifacts.node-1.internal")
    );
    assert_eq!(config.admin.auth_token, Some("secret-token".to_string()));
    assert_eq!(config.runtime.key_source, "file");
    assert_eq!(
        config.runtime.cache_directory.as_deref(),
        Some("/var/cache/wasm-node/wasmtime")
    );
    assert!(config.runtime.pooling_allocator);
    assert_eq!(config.runtime.pooling_total_component_instances, 128);
    assert_eq!(
        config.runtime.pooling_max_core_instances_per_component,
        Some(16)
    );
    assert_eq!(config.runtime.pooling_max_memories_per_component, Some(8));
    assert_eq!(config.runtime.pooling_max_tables_per_component, Some(8));
    assert!(config.database.enable_db_proxy);
    assert_eq!(config.database.db_proxy_max_connections, 50);
    assert_eq!(config.logging.level, "warn");
    assert_eq!(
        config.billing.export_dir,
        Some("/var/lib/wasm-node/billing".to_string())
    );
    assert_eq!(config.gc.disk_warning_threshold, 0.85);
    assert_eq!(config.rate_limit.default_requests_per_second, 5000);
    assert_eq!(config.ebpf.fd_soft_limit, 8192);
    assert_eq!(
        config.dns.platform_domain,
        Some("myplatform.com".to_string())
    );
    assert_eq!(config.health.check_interval_secs, 10);
    assert_eq!(config.health.default_fuel_quota, 1000000000);
}

#[test]
fn test_merge_priority_env_over_toml() {
    let mut config = NodeConfig::default();
    config.node.node_id = "from-toml".to_string();
    config.nats.url = "nats://toml:4222".to_string();

    std::env::set_var("WASM_NODE_NODE_ID", "from-env");
    std::env::set_var("WASM_NODE_NATS_URL", "nats://env:4222");
    std::env::set_var("WASM_NODE_ADMIN_ADVERTISED_HOST", "node-env.internal");
    std::env::set_var("WASM_NODE_ADMIN_BIND_ADDRESS", "0.0.0.0");
    std::env::set_var("WASM_NODE_ADMIN_TLS_CERT", "/tmp/admin.crt");
    std::env::set_var("WASM_NODE_RUNTIME_CACHE_DIRECTORY", "/tmp/wasmtime-cache");
    std::env::set_var(
        "WASM_NODE_RUNTIME_UPGRADE_SIGNING_PUBLIC_KEY",
        "1111111111111111111111111111111111111111111111111111111111111111",
    );
    std::env::set_var("WASM_NODE_RUNTIME_POOLING_ALLOCATOR", "true");
    std::env::set_var(
        "WASM_NODE_STORAGE_OPEN_FAILURE_MODE",
        "quarantine_and_recreate",
    );
    let config = apply_env_overrides(config);
    std::env::remove_var("WASM_NODE_NODE_ID");
    std::env::remove_var("WASM_NODE_NATS_URL");
    std::env::remove_var("WASM_NODE_ADMIN_ADVERTISED_HOST");
    std::env::remove_var("WASM_NODE_ADMIN_BIND_ADDRESS");
    std::env::remove_var("WASM_NODE_ADMIN_TLS_CERT");
    std::env::remove_var("WASM_NODE_RUNTIME_CACHE_DIRECTORY");
    std::env::remove_var("WASM_NODE_RUNTIME_UPGRADE_SIGNING_PUBLIC_KEY");
    std::env::remove_var("WASM_NODE_RUNTIME_POOLING_ALLOCATOR");
    std::env::remove_var("WASM_NODE_STORAGE_OPEN_FAILURE_MODE");

    assert_eq!(config.node.node_id, "from-env");
    assert_eq!(config.nats.url, "nats://env:4222");
    assert_eq!(
        config.admin.advertised_host.as_deref(),
        Some("node-env.internal")
    );
    assert_eq!(config.admin.bind_address, "0.0.0.0");
    assert_eq!(config.admin.tls_cert.as_deref(), Some("/tmp/admin.crt"));
    assert_eq!(
        config.runtime.cache_directory.as_deref(),
        Some("/tmp/wasmtime-cache")
    );
    assert_eq!(
        config.runtime.upgrade_signing_public_key.as_deref(),
        Some("1111111111111111111111111111111111111111111111111111111111111111")
    );
    assert!(config.runtime.pooling_allocator);
    assert_eq!(
        config.storage.open_failure_mode,
        StorageOpenFailureMode::QuarantineAndRecreate
    );
}

#[test]
fn test_merge_priority_cli_over_env() {
    let mut config = NodeConfig::default();
    config.node.node_id = "from-env".to_string();
    config.proxy.http_port = 8080;

    let cli = CliOverrides {
        node_id: Some("from-cli".to_string()),
        http_port: Some(9090),
        admin_bind_address: Some("::1".to_string()),
        artifact_bind_address: Some("0.0.0.0".to_string()),
        admin_tls_key: Some("/tmp/admin.key".to_string()),
        runtime_cache_directory: Some("/tmp/cli-wasmtime-cache".to_string()),
        runtime_upgrade_signing_public_key: Some(
            "2222222222222222222222222222222222222222222222222222222222222222".to_string(),
        ),
        runtime_pooling_total_component_instances: Some(256),
        runtime_pooling_max_tables_per_component: Some(12),
        admin_advertised_artifact_url: Some("https://cli-artifacts.internal".to_string()),
        ..Default::default()
    };
    let config = apply_cli_overrides(config, &cli);

    assert_eq!(config.node.node_id, "from-cli");
    assert_eq!(config.proxy.http_port, 9090);
    assert_eq!(config.admin.bind_address, "::1");
    assert_eq!(config.admin.artifact_bind_address, "0.0.0.0");
    assert_eq!(config.admin.tls_key.as_deref(), Some("/tmp/admin.key"));
    assert_eq!(
        config.runtime.cache_directory.as_deref(),
        Some("/tmp/cli-wasmtime-cache")
    );
    assert_eq!(
        config.runtime.upgrade_signing_public_key.as_deref(),
        Some("2222222222222222222222222222222222222222222222222222222222222222")
    );
    assert_eq!(config.runtime.pooling_total_component_instances, 256);
    assert_eq!(config.runtime.pooling_max_tables_per_component, Some(12));
    assert_eq!(
        config.admin.advertised_artifact_url.as_deref(),
        Some("https://cli-artifacts.internal")
    );
}

#[test]
fn test_validation_port_range_swapped() {
    let mut config = NodeConfig::default();
    config.runtime.port_start = 20000;
    config.runtime.port_end = 10000;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("port_start must be less than port_end"));
}

#[test]
fn test_validation_port_range_too_small() {
    let mut config = NodeConfig::default();
    config.runtime.port_start = 10000;
    config.runtime.port_end = 10050;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("port range must span at least 100 ports"));
}

#[test]
fn test_validation_invalid_log_level() {
    let mut config = NodeConfig::default();
    config.logging.level = "verbose".to_string();
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("invalid log level"));
}

#[test]
fn test_validation_tls_consistency() {
    let mut config = NodeConfig::default();
    config.proxy.tls_cert = Some("/path/to/cert".to_string());
    config.proxy.tls_key = None;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("tls_cert and tls_key must both be set or both be unset"));
}

#[test]
fn test_validation_https_without_tls() {
    let mut config = NodeConfig::default();
    config.proxy.https_port = 443;
    config.proxy.tls_cert = None;
    config.proxy.tls_key = None;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("https_port requires tls_cert and tls_key"));
}

#[test]
fn test_validation_rejects_invalid_upgrade_signing_public_key() {
    let mut config = NodeConfig::default();
    config.runtime.upgrade_signing_public_key = Some("not-hex".to_string());
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("runtime.upgrade_signing_public_key is not valid hex"));
}

#[test]
fn test_validation_rejects_zero_pooling_total_component_instances() {
    let mut config = NodeConfig::default();
    config.runtime.pooling_total_component_instances = 0;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("runtime.pooling_total_component_instances must be > 0"));
}

#[test]
fn test_validation_rejects_zero_pooling_component_caps_when_enabled() {
    let mut config = NodeConfig::default();
    config.runtime.pooling_allocator = true;
    config.runtime.pooling_max_tables_per_component = Some(0);
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("runtime.pooling_max_tables_per_component must be > 0"));
}

#[test]
fn test_validation_rejects_partial_admin_tls_config() {
    let mut config = NodeConfig::default();
    config.admin.tls_cert = Some("/tmp/admin.crt".to_string());
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("admin.tls_cert and admin.tls_key must both be set or both be unset"));
}

#[test]
fn test_validation_rejects_require_tls_without_any_tls_material() {
    let mut config = NodeConfig::default();
    config.auth.enabled = true;
    config.auth.require_tls = true;
    config.proxy.tls_cert = None;
    config.proxy.tls_key = None;
    config.admin.tls_cert = None;
    config.admin.tls_key = None;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("auth.require_tls = true requires either admin.tls_cert/admin.tls_key or proxy.tls_cert/proxy.tls_key"));
}

#[test]
fn test_validation_rejects_bind_address_with_port() {
    let mut config = NodeConfig::default();
    config.admin.bind_address = "127.0.0.1:9090".to_string();
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("admin.bind_address must not include a port"));
}

#[test]
fn test_validation_rejects_bind_address_url() {
    let mut config = NodeConfig::default();
    config.admin.artifact_bind_address = "http://127.0.0.1".to_string();
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("admin.artifact_bind_address must be a host/IP only, not a URL"));
}

#[test]
fn test_validation_rejects_loopback_advertised_host() {
    let mut config = NodeConfig::default();
    config.admin.advertised_host = Some("127.0.0.1".to_string());
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("admin.advertised_host must not be loopback"));
}

#[test]
fn test_validation_rejects_loopback_advertised_artifact_url() {
    let mut config = NodeConfig::default();
    config.admin.advertised_artifact_url = Some("http://127.0.0.1:9091".to_string());
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("admin.advertised_artifact_url must not use a loopback host"));
}

#[test]
fn test_validation_accepts_routable_advertised_artifact_config() {
    let mut config = NodeConfig::default();
    config.admin.advertised_host = Some("node-1.internal".to_string());
    config.admin.advertised_artifact_url =
        Some("https://artifacts.node-1.internal/base".to_string());
    assert!(validate_config(&config).is_ok());
}

#[test]
fn test_validation_rejects_public_artifact_bind_without_write_auth() {
    let mut config = NodeConfig::default();
    config.admin.artifact_bind_address = "10.0.0.5".to_string();
    config.admin.auth_token = None;
    config.auth.write_token = None;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("admin.artifact_bind_address"));
    assert!(msg.contains("auth.write_token"));
}

#[test]
fn test_validation_accepts_public_artifact_bind_with_write_auth() {
    let mut config = NodeConfig::default();
    config.admin.artifact_bind_address = "10.0.0.5".to_string();
    config.auth.write_token = Some("valid_write_token_1234567890".to_string());
    assert!(validate_config(&config).is_ok());
}

#[test]
fn test_runtime_instance_bind_address_defaults_to_loopback() {
    let config = NodeConfig::default();
    assert_eq!(config.runtime.instance_bind_address, "127.0.0.1");
}

#[test]
fn test_validation_rejects_invalid_runtime_instance_bind_address() {
    let mut config = NodeConfig::default();
    config.runtime.instance_bind_address = "not-an-ip".to_string();
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("runtime.instance_bind_address"));
    assert!(msg.contains("IP address literal"));
}

#[test]
fn test_validation_rejects_command_key_source_without_argv() {
    let mut config = NodeConfig::default();
    config.runtime.key_source = "command".to_string();
    config.runtime.key_command.clear();
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("runtime.key_command"));
}

#[test]
fn test_validation_accepts_command_key_source_with_argv() {
    let mut config = NodeConfig::default();
    config.runtime.key_source = "command".to_string();
    config.runtime.key_command = vec![
        "/usr/local/bin/fetch-node-seal-key".to_string(),
        "--node".to_string(),
        "prod-node-0".to_string(),
    ];
    assert!(validate_config(&config).is_ok());
}

#[test]
fn test_validation_rejects_vault_kv_key_source_without_required_fields() {
    let mut config = NodeConfig::default();
    config.runtime.key_source = "vault-kv".to_string();
    config.runtime.key_vault_url = None;
    config.runtime.key_vault_token_env = None;
    config.runtime.key_vault_path = None;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("runtime.key_vault_url"));
    assert!(msg.contains("runtime.key_vault_token_env"));
    assert!(msg.contains("runtime.key_vault_path"));
}

#[test]
fn test_validation_accepts_vault_kv_key_source_with_required_fields() {
    let mut config = NodeConfig::default();
    config.runtime.key_source = "vault-kv".to_string();
    config.runtime.key_vault_url = Some("https://vault.service:8200".to_string());
    config.runtime.key_vault_token_env = Some("VAULT_TOKEN".to_string());
    config.runtime.key_vault_path = Some("wasm-node/seal-key".to_string());
    config.runtime.key_vault_mount = "secret".to_string();
    config.runtime.key_vault_field = "key".to_string();
    assert!(validate_config(&config).is_ok());
}

#[test]
fn test_validation_rejects_vault_transit_key_source_without_required_fields() {
    let mut config = NodeConfig::default();
    config.runtime.key_source = "vault-transit".to_string();
    config.runtime.key_vault_url = None;
    config.runtime.key_vault_token_env = None;
    config.runtime.key_vault_transit_key = None;
    config.runtime.key_vault_transit_context = None;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("runtime.key_vault_url"));
    assert!(msg.contains("runtime.key_vault_token_env"));
    assert!(msg.contains("runtime.key_vault_transit_key"));
    assert!(msg.contains("runtime.key_vault_transit_context"));
}

#[test]
fn test_validation_accepts_vault_transit_key_source_with_required_fields() {
    let mut config = NodeConfig::default();
    config.runtime.key_source = "vault-transit".to_string();
    config.runtime.key_vault_url = Some("https://vault.service:8200".to_string());
    config.runtime.key_vault_token_env = Some("VAULT_TOKEN".to_string());
    config.runtime.key_vault_transit_mount = "transit".to_string();
    config.runtime.key_vault_transit_key = Some("wasm-node-seal".to_string());
    config.runtime.key_vault_transit_context = Some("prod-node-0".to_string());
    assert!(validate_config(&config).is_ok());
}

#[test]
fn test_validation_rejects_aws_kms_hmac_key_source_without_required_fields() {
    let mut config = NodeConfig::default();
    config.runtime.key_source = "aws-kms-hmac".to_string();
    config.runtime.key_aws_kms_region = None;
    config.runtime.key_aws_kms_key_id = None;
    config.runtime.key_aws_kms_context = None;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("runtime.key_aws_kms_region"));
    assert!(msg.contains("runtime.key_aws_kms_key_id"));
    assert!(msg.contains("runtime.key_aws_kms_context"));
}

#[test]
fn test_validation_accepts_aws_kms_hmac_key_source_with_required_fields() {
    let mut config = NodeConfig::default();
    config.runtime.key_source = "aws-kms-hmac".to_string();
    config.runtime.key_aws_kms_region = Some("eu-west-3".to_string());
    config.runtime.key_aws_kms_endpoint = Some("http://127.0.0.1:4566".to_string());
    config.runtime.key_aws_kms_key_id =
        Some("arn:aws:kms:eu-west-3:123456789012:key/test".to_string());
    config.runtime.key_aws_kms_context = Some("prod-node-0".to_string());
    assert!(validate_config(&config).is_ok());
}

#[test]
fn test_validation_ebpf_fd_limits() {
    let mut config = NodeConfig::default();
    config.ebpf.fd_soft_limit = 10000;
    config.ebpf.fd_hard_limit = 8000;
    let result = validate_config(&config);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("fd_soft_limit must be less than fd_hard_limit"));
}

#[test]
fn test_hot_config_update_partial() {
    let base = HotConfig::from_cold_config(&NodeConfig::default());
    let original_level = base.logging.level.clone();

    let update = HotConfigUpdate {
        rate_limit_default_rps: Some(9999),
        ..Default::default()
    };

    let updated = merge_hot_config_update(&base, update);
    assert_eq!(updated.rate_limit.default_requests_per_second, 9999);
    assert_eq!(
        updated.rate_limit.default_burst_capacity,
        base.rate_limit.default_burst_capacity
    );
    assert_eq!(
        updated.rate_limit.default_per_ip_limit,
        base.rate_limit.default_per_ip_limit
    );
    assert_eq!(updated.logging.level, original_level);
    assert_eq!(updated.ebpf.fd_soft_limit, base.ebpf.fd_soft_limit);
}

#[test]
fn test_hot_config_validation_invalid_log_level() {
    let mut config = HotConfig::from_cold_config(&NodeConfig::default());
    config.logging.level = "invalid".to_string();
    assert!(validate_hot(&config).is_err());
}

#[test]
fn test_hot_config_validation_fd_limits() {
    let mut config = HotConfig::from_cold_config(&NodeConfig::default());
    config.ebpf.fd_soft_limit = 10000;
    config.ebpf.fd_hard_limit = 8000;
    assert!(validate_hot(&config).is_err());
}

#[test]
fn test_hot_config_validation_disk_threshold() {
    let mut config = HotConfig::from_cold_config(&NodeConfig::default());
    config.gc.disk_warning_threshold = 1.5;
    assert!(validate_hot(&config).is_err());

    config.gc.disk_warning_threshold = 0.0;
    assert!(validate_hot(&config).is_err());
}

#[test]
fn test_hot_config_validation_valid() {
    let config = HotConfig::from_cold_config(&NodeConfig::default());
    assert!(validate_hot(&config).is_ok());
}

#[test]
fn test_hot_config_update_count_changes() {
    let update = HotConfigUpdate {
        rate_limit_default_rps: Some(5000),
        gc_interval_secs: Some(120),
        logging_level: Some("debug".to_string()),
        ..Default::default()
    };
    assert_eq!(update.count_changes(), 3);

    let empty = HotConfigUpdate::default();
    assert_eq!(empty.count_changes(), 0);
}

#[test]
fn test_hot_config_from_cold() {
    let mut cold = NodeConfig::default();
    cold.rate_limit.default_requests_per_second = 5000;
    cold.ebpf.fd_soft_limit = 4096;
    cold.gc.gc_interval_secs = 120;
    cold.health.check_interval_secs = 10;
    cold.logging.level = "debug".to_string();

    let hot = HotConfig::from_cold_config(&cold);
    assert_eq!(hot.rate_limit.default_requests_per_second, 5000);
    assert_eq!(hot.ebpf.fd_soft_limit, 4096);
    assert_eq!(hot.gc.gc_interval_secs, 120);
    assert_eq!(hot.health.check_interval_secs, 10);
    assert_eq!(hot.logging.level, "debug");
}

#[test]
fn test_merge_config_option_fields() {
    let mut base = NodeConfig::default();
    base.nats.creds_file = Some("base-creds".to_string());
    base.proxy.tls_cert = Some("base-cert".to_string());
    base.proxy.tls_key = Some("base-key".to_string());

    let mut overlay = NodeConfig::default();
    overlay.nats.creds_file = None;
    overlay.proxy.tls_cert = Some("overlay-cert".to_string());
    overlay.proxy.tls_key = Some("overlay-key".to_string());

    let merged = merge_config(base, overlay);
    assert_eq!(merged.nats.creds_file, Some("base-creds".to_string()));
    assert_eq!(merged.proxy.tls_cert, Some("overlay-cert".to_string()));
    assert_eq!(merged.proxy.tls_key, Some("overlay-key".to_string()));
}

#[test]
fn test_load_config_defaults_only() {
    let result = load_config(None, &CliOverrides::default());
    assert!(result.is_ok());
    let config = result.unwrap();
    assert_eq!(config.node.node_id, "node-0");
    assert_eq!(config.proxy.http_port, 8080);
}

#[test]
fn test_load_config_missing_file() {
    let result = load_config(
        Some(std::path::Path::new("/nonexistent/config.toml")),
        &CliOverrides::default(),
    );
    assert!(result.is_ok());
}

#[test]
fn test_load_config_with_cli_overrides() {
    let cli = CliOverrides {
        node_id: Some("cli-node".to_string()),
        http_port: Some(3000),
        log_level: Some("trace".to_string()),
        ..Default::default()
    };
    let result = load_config(None, &cli);
    assert!(result.is_ok());
    let config = result.unwrap();
    assert_eq!(config.node.node_id, "cli-node");
    assert_eq!(config.proxy.http_port, 3000);
    assert_eq!(config.logging.level, "trace");
}
