use crate::overrides::CliOverrides;
use crate::validation::validate_config;
use common::config::{
    AdminSection, AuthSection, BillingSection, DatabaseSection, DeploymentEnvironment, DnsSection,
    EbpfSection, GatewayCircuitBreakerSection, GatewayRateLimitSection, GatewaySection, GcSection,
    HealthSection, LoggingSection, NatsSection, NodeConfig, NodeSection, ProxySection,
    RateLimitSection, RuntimeSection, StorageIntegrityFailureMode, StorageOpenFailureMode,
    StorageSection,
};
use common::error::PlatformError;
use std::path::{Path, PathBuf};

/// Load configuration with the merge priority:
/// 1. built-in defaults
/// 2. TOML file
/// 3. environment variables
/// 4. CLI flags
pub fn load_config(
    config_path: Option<&Path>,
    cli_overrides: &CliOverrides,
) -> Result<NodeConfig, PlatformError> {
    let mut config = NodeConfig::default();

    if let Some(path) = config_path {
        if path.exists() {
            let toml_str = std::fs::read_to_string(path).map_err(|e| {
                PlatformError::ConfigValidation(format!(
                    "failed to read config file {}: {}",
                    path.display(),
                    e
                ))
            })?;
            let file_config: NodeConfig = toml::from_str(&toml_str).map_err(|e| {
                PlatformError::ConfigValidation(format!(
                    "failed to parse config file {}: {}",
                    path.display(),
                    e
                ))
            })?;
            config = merge_config(config, file_config);
            tracing::info!(path = %path.display(), "configuration file loaded");
        } else {
            tracing::warn!(path = %path.display(), "config file not found, using defaults");
        }
    }

    config = apply_env_overrides(config);
    config = apply_cli_overrides(config, cli_overrides);
    validate_config(&config)?;
    Ok(config)
}

/// Merge two configs where the overlay wins.
pub(crate) fn merge_config(base: NodeConfig, overlay: NodeConfig) -> NodeConfig {
    NodeConfig {
        node: NodeSection {
            node_id: overlay.node.node_id,
            environment: overlay.node.environment,
        },
        storage: StorageSection {
            db_path: overlay.storage.db_path,
            open_failure_mode: overlay.storage.open_failure_mode,
            integrity_failure_mode: overlay.storage.integrity_failure_mode,
        },
        nats: NatsSection {
            url: overlay.nats.url,
            creds_file: overlay.nats.creds_file.or(base.nats.creds_file),
            ca_cert: overlay.nats.ca_cert.or(base.nats.ca_cert),
            client_cert: overlay.nats.client_cert.or(base.nats.client_cert),
            client_key: overlay.nats.client_key.or(base.nats.client_key),
        },
        proxy: ProxySection {
            http_port: overlay.proxy.http_port,
            https_port: overlay.proxy.https_port,
            tls_cert: overlay.proxy.tls_cert.or(base.proxy.tls_cert),
            tls_key: overlay.proxy.tls_key.or(base.proxy.tls_key),
        },
        admin: AdminSection {
            port: overlay.admin.port,
            artifact_port: overlay.admin.artifact_port,
            deploy_ingress_port: overlay.admin.deploy_ingress_port,
            bind_address: overlay.admin.bind_address,
            artifact_bind_address: overlay.admin.artifact_bind_address,
            deploy_ingress_bind_address: overlay.admin.deploy_ingress_bind_address,
            tls_cert: overlay.admin.tls_cert.or(base.admin.tls_cert),
            tls_key: overlay.admin.tls_key.or(base.admin.tls_key),
            advertised_host: overlay.admin.advertised_host.or(base.admin.advertised_host),
            advertised_artifact_url: overlay
                .admin
                .advertised_artifact_url
                .or(base.admin.advertised_artifact_url),
            auth_token: overlay.admin.auth_token.or(base.admin.auth_token),
        },
        auth: AuthSection {
            enabled: overlay.auth.enabled,
            read_token: overlay.auth.read_token.or(base.auth.read_token),
            write_token: overlay.auth.write_token.or(base.auth.write_token),
            require_tls: overlay.auth.require_tls,
            rate_limit_per_second: overlay.auth.rate_limit_per_second,
            rate_limit_burst: overlay.auth.rate_limit_burst,
            trusted_proxies: if overlay.auth.trusted_proxies.is_empty() {
                base.auth.trusted_proxies.clone()
            } else {
                overlay.auth.trusted_proxies.clone()
            },
        },
        runtime: RuntimeSection {
            port_start: overlay.runtime.port_start,
            port_end: overlay.runtime.port_end,
            instance_bind_address: if overlay.runtime.instance_bind_address.is_empty() {
                base.runtime.instance_bind_address.clone()
            } else {
                overlay.runtime.instance_bind_address
            },
            isolation_mode: overlay
                .runtime
                .isolation_mode
                .or(base.runtime.isolation_mode),
            key_source: overlay.runtime.key_source,
            key_file: overlay.runtime.key_file.or(base.runtime.key_file),
            key_command: if overlay.runtime.key_command.is_empty() {
                base.runtime.key_command.clone()
            } else {
                overlay.runtime.key_command.clone()
            },
            key_vault_url: overlay.runtime.key_vault_url.or(base.runtime.key_vault_url),
            key_vault_token_env: overlay
                .runtime
                .key_vault_token_env
                .or(base.runtime.key_vault_token_env),
            key_vault_ca_cert: overlay
                .runtime
                .key_vault_ca_cert
                .or(base.runtime.key_vault_ca_cert),
            key_vault_mount: if overlay.runtime.key_vault_mount.is_empty() {
                base.runtime.key_vault_mount.clone()
            } else {
                overlay.runtime.key_vault_mount
            },
            key_vault_path: overlay
                .runtime
                .key_vault_path
                .or(base.runtime.key_vault_path),
            key_vault_field: if overlay.runtime.key_vault_field.is_empty() {
                base.runtime.key_vault_field.clone()
            } else {
                overlay.runtime.key_vault_field
            },
            key_vault_transit_mount: if overlay.runtime.key_vault_transit_mount.is_empty() {
                base.runtime.key_vault_transit_mount.clone()
            } else {
                overlay.runtime.key_vault_transit_mount
            },
            key_vault_transit_key: overlay
                .runtime
                .key_vault_transit_key
                .or(base.runtime.key_vault_transit_key),
            key_vault_transit_context: overlay
                .runtime
                .key_vault_transit_context
                .or(base.runtime.key_vault_transit_context),
            key_vault_transit_key_version: overlay
                .runtime
                .key_vault_transit_key_version
                .or(base.runtime.key_vault_transit_key_version),
            key_vault_transit_previous_key_version: overlay
                .runtime
                .key_vault_transit_previous_key_version
                .or(base.runtime.key_vault_transit_previous_key_version),
            key_aws_kms_region: overlay
                .runtime
                .key_aws_kms_region
                .or(base.runtime.key_aws_kms_region),
            key_aws_kms_endpoint: overlay
                .runtime
                .key_aws_kms_endpoint
                .or(base.runtime.key_aws_kms_endpoint),
            key_aws_kms_key_id: overlay
                .runtime
                .key_aws_kms_key_id
                .or(base.runtime.key_aws_kms_key_id),
            key_aws_kms_previous_key_id: overlay
                .runtime
                .key_aws_kms_previous_key_id
                .or(base.runtime.key_aws_kms_previous_key_id),
            key_aws_kms_context: overlay
                .runtime
                .key_aws_kms_context
                .or(base.runtime.key_aws_kms_context),
            cache_directory: overlay
                .runtime
                .cache_directory
                .or(base.runtime.cache_directory),
            upgrade_signing_public_key: overlay
                .runtime
                .upgrade_signing_public_key
                .or(base.runtime.upgrade_signing_public_key),
            pooling_allocator: overlay.runtime.pooling_allocator,
            pooling_total_component_instances: overlay.runtime.pooling_total_component_instances,
            pooling_max_core_instances_per_component: overlay
                .runtime
                .pooling_max_core_instances_per_component
                .or(base.runtime.pooling_max_core_instances_per_component),
            pooling_max_memories_per_component: overlay
                .runtime
                .pooling_max_memories_per_component
                .or(base.runtime.pooling_max_memories_per_component),
            pooling_max_tables_per_component: overlay
                .runtime
                .pooling_max_tables_per_component
                .or(base.runtime.pooling_max_tables_per_component),
        },
        database: DatabaseSection {
            default_url: overlay.database.default_url,
            pgbouncer_addr: overlay.database.pgbouncer_addr,
            enable_db_proxy: overlay.database.enable_db_proxy,
            db_proxy_addr: overlay.database.db_proxy_addr,
            db_proxy_backend: overlay.database.db_proxy_backend,
            db_proxy_max_connections: overlay.database.db_proxy_max_connections,
        },
        logging: LoggingSection {
            level: overlay.logging.level,
            format: overlay.logging.format,
            output: overlay.logging.output.or(base.logging.output),
            otlp_endpoint: overlay.logging.otlp_endpoint.or(base.logging.otlp_endpoint),
            modules: if overlay.logging.modules.is_empty() {
                base.logging.modules.clone()
            } else {
                overlay.logging.modules.clone()
            },
            sampling: overlay.logging.sampling.clone(),
            rotation: overlay.logging.rotation.clone(),
            forward: overlay.logging.forward.clone(),
            audit: overlay.logging.audit.clone(),
        },
        billing: BillingSection {
            export_dir: overlay.billing.export_dir.or(base.billing.export_dir),
            export_interval_secs: overlay.billing.export_interval_secs,
        },
        gc: GcSection {
            artifact_keep_versions: overlay.gc.artifact_keep_versions,
            metrics_retain_days: overlay.gc.metrics_retain_days,
            undeploy_grace_secs: overlay.gc.undeploy_grace_secs,
            gc_interval_secs: overlay.gc.gc_interval_secs,
            disk_warning_threshold: overlay.gc.disk_warning_threshold,
        },
        rate_limit: RateLimitSection {
            default_requests_per_second: overlay.rate_limit.default_requests_per_second,
            default_burst_capacity: overlay.rate_limit.default_burst_capacity,
            default_per_ip_limit: overlay.rate_limit.default_per_ip_limit,
        },
        ebpf: EbpfSection {
            enabled: overlay.ebpf.enabled,
            required: overlay.ebpf.required,
            fd_soft_limit: overlay.ebpf.fd_soft_limit,
            fd_hard_limit: overlay.ebpf.fd_hard_limit,
            mem_low_threshold_pages: overlay.ebpf.mem_low_threshold_pages,
            mem_critical_threshold_pages: overlay.ebpf.mem_critical_threshold_pages,
            disk_slow_threshold_ns: overlay.ebpf.disk_slow_threshold_ns,
            tcp_conn_limit_per_pid: overlay.ebpf.tcp_conn_limit_per_pid,
            syscall_rate_limit: overlay.ebpf.syscall_rate_limit,
            sampling_period_secs: overlay.ebpf.sampling_period_secs,
            enable_namespace_enforcer: overlay.ebpf.enable_namespace_enforcer,
            gateway_port: overlay.ebpf.gateway_port,
            enable_forged_header_detect: overlay.ebpf.enable_forged_header_detect,
        },
        dns: DnsSection {
            platform_domain: overlay.dns.platform_domain.or(base.dns.platform_domain),
            webhook_url: overlay.dns.webhook_url.or(base.dns.webhook_url),
            webhook_token: overlay.dns.webhook_token.or(base.dns.webhook_token),
            stub_enabled: overlay.dns.stub_enabled,
            stub_port: overlay.dns.stub_port,
        },
        health: HealthSection {
            check_interval_secs: overlay.health.check_interval_secs,
            check_timeout_secs: overlay.health.check_timeout_secs,
            default_idle_timeout_secs: overlay.health.default_idle_timeout_secs,
            default_max_instances: overlay.health.default_max_instances,
            default_fuel_quota: overlay.health.default_fuel_quota,
            default_memory_pages: overlay.health.default_memory_pages,
            failure_threshold: overlay.health.failure_threshold,
            success_threshold: overlay.health.success_threshold,
            min_disk_free_bytes: overlay.health.min_disk_free_bytes,
            min_disk_free_inodes: overlay.health.min_disk_free_inodes,
            max_memory_bytes: overlay.health.max_memory_bytes,
            snapshot_interval_secs: overlay.health.snapshot_interval_secs,
            cluster_node_stale_after_secs: overlay.health.cluster_node_stale_after_secs,
            app_defaults: overlay.health.app_defaults.clone(),
        },
        gateway: GatewaySection {
            oidc: overlay.gateway.oidc.or(base.gateway.oidc),
            rate_limit: GatewayRateLimitSection {
                kv_bucket: if overlay.gateway.rate_limit.kv_bucket.is_empty() {
                    base.gateway.rate_limit.kv_bucket.clone()
                } else {
                    overlay.gateway.rate_limit.kv_bucket.clone()
                },
                sync_interval_ms: overlay.gateway.rate_limit.sync_interval_ms,
            },
            circuit_breaker: GatewayCircuitBreakerSection {
                default_failure_threshold: overlay
                    .gateway
                    .circuit_breaker
                    .default_failure_threshold,
                default_reset_timeout_secs: overlay
                    .gateway
                    .circuit_breaker
                    .default_reset_timeout_secs,
            },
        },
    }
}

pub(crate) fn apply_env_overrides(mut config: NodeConfig) -> NodeConfig {
    if let Ok(value) = std::env::var("WASM_NODE_ENVIRONMENT") {
        config.node.environment = match value.trim().to_ascii_lowercase().as_str() {
            "development" => DeploymentEnvironment::Development,
            "test" => DeploymentEnvironment::Test,
            "production" => DeploymentEnvironment::Production,
            other => {
                tracing::warn!(value = other, "ignoring invalid WASM_NODE_ENVIRONMENT");
                config.node.environment
            }
        };
    }
    if let Ok(v) = std::env::var("WASM_NODE_NODE_ID") {
        config.node.node_id = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_STORAGE_DB_PATH") {
        config.storage.db_path = PathBuf::from(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_STORAGE_OPEN_FAILURE_MODE") {
        match v.trim() {
            "quarantine_and_fail" => {
                config.storage.open_failure_mode = StorageOpenFailureMode::QuarantineAndFail
            }
            "quarantine_and_recreate" => {
                config.storage.open_failure_mode = StorageOpenFailureMode::QuarantineAndRecreate
            }
            other => tracing::warn!(
                value = other,
                "ignoring invalid WASM_NODE_STORAGE_OPEN_FAILURE_MODE"
            ),
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_STORAGE_INTEGRITY_FAILURE_MODE") {
        match v.trim() {
            "quarantine_and_exit" => {
                config.storage.integrity_failure_mode =
                    StorageIntegrityFailureMode::QuarantineAndExit
            }
            "delete_and_exit" => {
                config.storage.integrity_failure_mode = StorageIntegrityFailureMode::DeleteAndExit
            }
            other => tracing::warn!(
                value = other,
                "ignoring invalid WASM_NODE_STORAGE_INTEGRITY_FAILURE_MODE"
            ),
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_NATS_URL") {
        config.nats.url = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_NATS_CREDS_FILE") {
        config.nats.creds_file = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_NATS_CA_CERT") {
        config.nats.ca_cert = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_NATS_CLIENT_CERT") {
        config.nats.client_cert = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_NATS_CLIENT_KEY") {
        config.nats.client_key = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_HTTP_PORT") {
        if let Ok(port) = v.parse() {
            config.proxy.http_port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_HTTPS_PORT") {
        if let Ok(port) = v.parse() {
            config.proxy.https_port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_TLS_CERT") {
        config.proxy.tls_cert = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_TLS_KEY") {
        config.proxy.tls_key = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_PORT") {
        if let Ok(port) = v.parse() {
            config.admin.port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_AUTH_TOKEN") {
        config.admin.auth_token = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_BIND_ADDRESS") {
        config.admin.bind_address = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_ARTIFACT_BIND_ADDRESS") {
        config.admin.artifact_bind_address = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_TLS_CERT") {
        config.admin.tls_cert = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_TLS_KEY") {
        config.admin.tls_key = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_ADVERTISED_HOST") {
        config.admin.advertised_host = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_ADVERTISED_ARTIFACT_URL") {
        config.admin.advertised_artifact_url = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_LOGGING_LEVEL") {
        config.logging.level = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_LOGGING_OTLP_ENDPOINT") {
        config.logging.otlp_endpoint = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_REQUESTS_PER_SECOND") {
        if let Ok(rps) = v.parse() {
            config.rate_limit.default_requests_per_second = rps;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_BURST_CAPACITY") {
        if let Ok(burst) = v.parse() {
            config.rate_limit.default_burst_capacity = burst;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_PER_IP_LIMIT") {
        if let Ok(limit) = v.parse() {
            config.rate_limit.default_per_ip_limit = limit;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_EBPF_ENABLED") {
        if let Ok(enabled) = v.parse() {
            config.ebpf.enabled = enabled;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_AUTH_ENABLED") {
        if let Ok(enabled) = v.parse() {
            config.auth.enabled = enabled;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_AUTH_READ_TOKEN") {
        config.auth.read_token = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_AUTH_WRITE_TOKEN") {
        config.auth.write_token = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_AUTH_REQUIRE_TLS") {
        if let Ok(require) = v.parse() {
            config.auth.require_tls = require;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_AUTH_RATE_LIMIT_PER_SECOND") {
        if let Ok(rps) = v.parse() {
            config.auth.rate_limit_per_second = rps;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_AUTH_RATE_LIMIT_BURST") {
        if let Ok(burst) = v.parse() {
            config.auth.rate_limit_burst = burst;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_AUTH_TRUSTED_PROXIES") {
        config.auth.trusted_proxies = v
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_CACHE_DIRECTORY") {
        config.runtime.cache_directory = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_COMMAND") {
        match serde_json::from_str::<Vec<String>>(&v) {
            Ok(argv) => config.runtime.key_command = argv,
            Err(err) => tracing::warn!(
                error = %err,
                "ignoring invalid WASM_NODE_RUNTIME_KEY_COMMAND JSON array"
            ),
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_URL") {
        config.runtime.key_vault_url = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_TOKEN_ENV") {
        config.runtime.key_vault_token_env = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_CA_CERT") {
        config.runtime.key_vault_ca_cert = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_MOUNT") {
        config.runtime.key_vault_mount = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_PATH") {
        config.runtime.key_vault_path = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_FIELD") {
        config.runtime.key_vault_field = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_TRANSIT_MOUNT") {
        config.runtime.key_vault_transit_mount = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_TRANSIT_KEY") {
        config.runtime.key_vault_transit_key = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_TRANSIT_CONTEXT") {
        config.runtime.key_vault_transit_context = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_TRANSIT_KEY_VERSION") {
        if let Ok(version) = v.parse() {
            config.runtime.key_vault_transit_key_version = Some(version);
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_VAULT_TRANSIT_PREVIOUS_KEY_VERSION") {
        if let Ok(version) = v.parse() {
            config.runtime.key_vault_transit_previous_key_version = Some(version);
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_AWS_KMS_REGION") {
        config.runtime.key_aws_kms_region = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_AWS_KMS_ENDPOINT") {
        config.runtime.key_aws_kms_endpoint = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_AWS_KMS_KEY_ID") {
        config.runtime.key_aws_kms_key_id = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_AWS_KMS_PREVIOUS_KEY_ID") {
        config.runtime.key_aws_kms_previous_key_id = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_KEY_AWS_KMS_CONTEXT") {
        config.runtime.key_aws_kms_context = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_UPGRADE_SIGNING_PUBLIC_KEY") {
        config.runtime.upgrade_signing_public_key = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_POOLING_ALLOCATOR") {
        if let Ok(enabled) = v.parse() {
            config.runtime.pooling_allocator = enabled;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_INSTANCE_BIND_ADDRESS") {
        config.runtime.instance_bind_address = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_ISOLATION_MODE") {
        config.runtime.isolation_mode = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_POOLING_TOTAL_COMPONENT_INSTANCES") {
        if let Ok(count) = v.parse() {
            config.runtime.pooling_total_component_instances = count;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_POOLING_MAX_CORE_INSTANCES_PER_COMPONENT") {
        if let Ok(count) = v.parse() {
            config.runtime.pooling_max_core_instances_per_component = Some(count);
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_POOLING_MAX_MEMORIES_PER_COMPONENT") {
        if let Ok(count) = v.parse() {
            config.runtime.pooling_max_memories_per_component = Some(count);
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RUNTIME_POOLING_MAX_TABLES_PER_COMPONENT") {
        if let Ok(count) = v.parse() {
            config.runtime.pooling_max_tables_per_component = Some(count);
        }
    }
    config
}

/// Apply CLI flag overrides. Only non-`None` values override.
pub(crate) fn apply_cli_overrides(mut config: NodeConfig, cli: &CliOverrides) -> NodeConfig {
    if let Some(v) = &cli.node_id {
        config.node.node_id = v.clone();
    }
    if let Some(v) = cli.environment {
        config.node.environment = v;
    }
    if let Some(v) = &cli.db_path {
        config.storage.db_path = PathBuf::from(v);
    }
    if let Some(v) = &cli.nats_url {
        config.nats.url = v.clone();
    }
    if let Some(v) = &cli.nats_creds {
        config.nats.creds_file = Some(v.clone());
    }
    if let Some(v) = &cli.nats_ca_cert {
        config.nats.ca_cert = Some(v.clone());
    }
    if let Some(v) = &cli.nats_client_cert {
        config.nats.client_cert = Some(v.clone());
    }
    if let Some(v) = &cli.nats_client_key {
        config.nats.client_key = Some(v.clone());
    }
    if let Some(v) = cli.http_port {
        config.proxy.http_port = v;
    }
    if let Some(v) = cli.https_port {
        config.proxy.https_port = v;
    }
    if let Some(v) = &cli.tls_cert {
        config.proxy.tls_cert = Some(v.clone());
    }
    if let Some(v) = &cli.tls_key {
        config.proxy.tls_key = Some(v.clone());
    }
    if let Some(v) = cli.admin_port {
        config.admin.port = v;
    }
    if let Some(v) = cli.artifact_port {
        config.admin.artifact_port = v;
    }
    if let Some(v) = cli.deploy_ingress_port {
        config.admin.deploy_ingress_port = v;
    }
    if let Some(v) = &cli.admin_bind_address {
        config.admin.bind_address = v.clone();
    }
    if let Some(v) = &cli.artifact_bind_address {
        config.admin.artifact_bind_address = v.clone();
    }
    if let Some(v) = &cli.deploy_ingress_bind_address {
        config.admin.deploy_ingress_bind_address = v.clone();
    }
    if let Some(v) = &cli.admin_tls_cert {
        config.admin.tls_cert = Some(v.clone());
    }
    if let Some(v) = &cli.admin_tls_key {
        config.admin.tls_key = Some(v.clone());
    }
    if let Some(v) = &cli.admin_advertised_host {
        config.admin.advertised_host = Some(v.clone());
    }
    if let Some(v) = &cli.admin_advertised_artifact_url {
        config.admin.advertised_artifact_url = Some(v.clone());
    }
    if let Some(v) = cli.port_start {
        config.runtime.port_start = v;
    }
    if let Some(v) = cli.port_end {
        config.runtime.port_end = v;
    }
    if let Some(v) = &cli.key_source {
        config.runtime.key_source = v.clone();
    }
    if let Some(v) = &cli.key_file {
        config.runtime.key_file = Some(v.clone());
    }
    if let Some(v) = &cli.key_command {
        config.runtime.key_command = v.clone();
    }
    if let Some(v) = &cli.key_vault_url {
        config.runtime.key_vault_url = Some(v.clone());
    }
    if let Some(v) = &cli.key_vault_token_env {
        config.runtime.key_vault_token_env = Some(v.clone());
    }
    if let Some(v) = &cli.key_vault_ca_cert {
        config.runtime.key_vault_ca_cert = Some(v.clone());
    }
    if let Some(v) = &cli.key_vault_mount {
        config.runtime.key_vault_mount = v.clone();
    }
    if let Some(v) = &cli.key_vault_path {
        config.runtime.key_vault_path = Some(v.clone());
    }
    if let Some(v) = &cli.key_vault_field {
        config.runtime.key_vault_field = v.clone();
    }
    if let Some(v) = &cli.key_vault_transit_mount {
        config.runtime.key_vault_transit_mount = v.clone();
    }
    if let Some(v) = &cli.key_vault_transit_key {
        config.runtime.key_vault_transit_key = Some(v.clone());
    }
    if let Some(v) = &cli.key_vault_transit_context {
        config.runtime.key_vault_transit_context = Some(v.clone());
    }
    if let Some(v) = &cli.key_aws_kms_region {
        config.runtime.key_aws_kms_region = Some(v.clone());
    }
    if let Some(v) = &cli.key_aws_kms_endpoint {
        config.runtime.key_aws_kms_endpoint = Some(v.clone());
    }
    if let Some(v) = &cli.key_aws_kms_key_id {
        config.runtime.key_aws_kms_key_id = Some(v.clone());
    }
    if let Some(v) = &cli.key_aws_kms_context {
        config.runtime.key_aws_kms_context = Some(v.clone());
    }
    if let Some(v) = &cli.runtime_cache_directory {
        config.runtime.cache_directory = Some(v.clone());
    }
    if let Some(v) = &cli.runtime_isolation_mode {
        config.runtime.isolation_mode = Some(v.clone());
    }
    if let Some(v) = &cli.runtime_upgrade_signing_public_key {
        config.runtime.upgrade_signing_public_key = Some(v.clone());
    }
    if let Some(v) = cli.runtime_pooling_allocator {
        config.runtime.pooling_allocator = v;
    }
    if let Some(v) = cli.runtime_pooling_total_component_instances {
        config.runtime.pooling_total_component_instances = v;
    }
    if let Some(v) = cli.runtime_pooling_max_core_instances_per_component {
        config.runtime.pooling_max_core_instances_per_component = Some(v);
    }
    if let Some(v) = cli.runtime_pooling_max_memories_per_component {
        config.runtime.pooling_max_memories_per_component = Some(v);
    }
    if let Some(v) = cli.runtime_pooling_max_tables_per_component {
        config.runtime.pooling_max_tables_per_component = Some(v);
    }
    if let Some(v) = &cli.database_url {
        config.database.default_url = v.clone();
    }
    if let Some(v) = &cli.pgbouncer_addr {
        config.database.pgbouncer_addr = v.clone();
    }
    if let Some(v) = cli.enable_db_proxy {
        config.database.enable_db_proxy = v;
    }
    if let Some(v) = &cli.db_proxy_addr {
        config.database.db_proxy_addr = v.clone();
    }
    if let Some(v) = &cli.db_proxy_backend {
        config.database.db_proxy_backend = v.clone();
    }
    if let Some(v) = cli.db_proxy_max_connections {
        config.database.db_proxy_max_connections = v;
    }
    if let Some(v) = &cli.log_level {
        config.logging.level = v.clone();
    }
    if let Some(v) = &cli.otlp_endpoint {
        config.logging.otlp_endpoint = Some(v.clone());
    }
    if let Some(v) = &cli.billing_export_dir {
        config.billing.export_dir = Some(v.clone());
    }
    if let Some(v) = cli.billing_export_interval_secs {
        config.billing.export_interval_secs = v;
    }
    if let Some(v) = &cli.platform_domain {
        config.dns.platform_domain = Some(v.clone());
    }
    if let Some(v) = &cli.dns_webhook_url {
        config.dns.webhook_url = Some(v.clone());
    }
    if let Some(v) = &cli.dns_webhook_token {
        config.dns.webhook_token = Some(v.clone());
    }
    if let Some(v) = &cli.auth_token {
        config.admin.auth_token = Some(v.clone());
    }
    if let Some(v) = cli.auth_enabled {
        config.auth.enabled = v;
    }
    if let Some(v) = &cli.auth_read_token {
        config.auth.read_token = Some(v.clone());
    }
    if let Some(v) = &cli.auth_write_token {
        config.auth.write_token = Some(v.clone());
    }
    if let Some(v) = cli.auth_require_tls {
        config.auth.require_tls = v;
    }
    if let Some(v) = cli.auth_rate_limit_per_second {
        config.auth.rate_limit_per_second = v;
    }
    if let Some(v) = cli.auth_rate_limit_burst {
        config.auth.rate_limit_burst = v;
    }
    config
}
