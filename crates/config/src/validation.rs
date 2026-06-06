use common::config::NodeConfig;
use common::error::PlatformError;
use std::net::IpAddr;
use url::Url;

fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed.eq_ignore_ascii_case("localhost")
        || trimmed
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn bind_address_is_loopback(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    !trimmed.is_empty() && is_loopback_host(trimmed)
}

fn validate_ip_literal(label: &str, value: &str, errors: &mut Vec<String>) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        errors.push(format!("{label} must not be empty"));
        return;
    }
    if trimmed.parse::<IpAddr>().is_err() {
        errors.push(format!("{label} must be an IP address literal"));
    }
}

fn effective_write_auth_token(config: &NodeConfig) -> Option<&str> {
    config
        .auth
        .write_token
        .as_deref()
        .or(config.admin.auth_token.as_deref())
}

fn validate_bind_address(label: &str, host: &str, errors: &mut Vec<String>) {
    let host = host.trim();
    let host_without_brackets = host.trim_start_matches('[').trim_end_matches(']');

    if host.is_empty() {
        errors.push(format!("{label} must not be empty"));
        return;
    }
    if host.contains("://") {
        errors.push(format!("{label} must be a host/IP only, not a URL"));
    }
    if host.contains('/') {
        errors.push(format!("{label} must not contain a path"));
    }
    if host.contains(':') && host_without_brackets.parse::<IpAddr>().is_err() {
        errors.push(format!("{label} must not include a port"));
    }
}

fn admin_tls_material_configured(config: &NodeConfig) -> bool {
    (config.admin.tls_cert.is_some() && config.admin.tls_key.is_some())
        || (config.proxy.tls_cert.is_some() && config.proxy.tls_key.is_some())
}

fn validate_admin_advertisement(config: &NodeConfig, errors: &mut Vec<String>) {
    if let Some(host) = config.admin.advertised_host.as_deref() {
        let host = host.trim();
        let host_without_brackets = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            errors.push("admin.advertised_host must not be empty".to_string());
        } else {
            if host.contains("://") {
                errors.push(
                    "admin.advertised_host must be a host/IP only; use admin.advertised_artifact_url for a full URL"
                        .to_string(),
                );
            }
            if host.contains('/') {
                errors.push("admin.advertised_host must not contain a path".to_string());
            }
            if host.contains(':') && host_without_brackets.parse::<IpAddr>().is_err() {
                errors.push(
                    "admin.advertised_host must not include a port; use artifact_port or admin.advertised_artifact_url"
                        .to_string(),
                );
            }
            if is_loopback_host(host) {
                errors.push(
                    "admin.advertised_host must not be loopback; leave it unset for local-only mode or set a routable host"
                        .to_string(),
                );
            }
        }
    }

    if let Some(raw_url) = config.admin.advertised_artifact_url.as_deref() {
        let raw_url = raw_url.trim();
        match Url::parse(raw_url) {
            Ok(url) => {
                if !matches!(url.scheme(), "http" | "https") {
                    errors.push("admin.advertised_artifact_url must use http or https".to_string());
                }
                if url.host_str().is_none() {
                    errors.push("admin.advertised_artifact_url must include a host".to_string());
                }
                if let Some(host) = url.host_str() {
                    if is_loopback_host(host) {
                        errors.push(
                            "admin.advertised_artifact_url must not use a loopback host; leave it unset for local-only mode or set a routable URL"
                                .to_string(),
                        );
                    }
                }
                if url.query().is_some() || url.fragment().is_some() {
                    errors.push(
                        "admin.advertised_artifact_url must not include a query string or fragment"
                            .to_string(),
                    );
                }
            }
            Err(e) => errors.push(format!(
                "admin.advertised_artifact_url is not a valid URL: {}",
                e
            )),
        }
    }
}

/// Validate the final merged configuration.
pub(crate) fn validate_config(config: &NodeConfig) -> Result<(), PlatformError> {
    let mut errors = Vec::new();

    if config.runtime.port_start >= config.runtime.port_end {
        errors.push("port_start must be less than port_end".to_string());
    } else if config.runtime.port_end - config.runtime.port_start < 100 {
        errors.push("port range must span at least 100 ports".to_string());
    }

    let valid_levels = ["trace", "debug", "info", "warn", "error"];
    if !valid_levels.contains(&config.logging.level.as_str()) {
        errors.push(format!(
            "invalid log level '{}', must be one of: {}",
            config.logging.level,
            valid_levels.join(", ")
        ));
    }

    if config.gc.disk_warning_threshold <= 0.0 || config.gc.disk_warning_threshold > 1.0 {
        errors.push("disk_warning_threshold must be between 0.0 and 1.0".to_string());
    }
    if config.gc.artifact_keep_versions == 0 {
        errors.push("artifact_keep_versions must be > 0".to_string());
    }

    if config.ebpf.fd_soft_limit >= config.ebpf.fd_hard_limit {
        errors.push("fd_soft_limit must be less than fd_hard_limit".to_string());
    }
    if config.ebpf.mem_low_threshold_pages <= config.ebpf.mem_critical_threshold_pages {
        errors.push(
            "mem_low_threshold_pages must be greater than mem_critical_threshold_pages".to_string(),
        );
    }

    if config.health.check_interval_secs == 0 {
        errors.push("check_interval_secs must be > 0".to_string());
    }
    if config.health.default_fuel_quota == 0 {
        errors.push("default_fuel_quota must be > 0".to_string());
    }
    if config.health.default_memory_pages == 0 {
        errors.push("default_memory_pages must be > 0".to_string());
    }

    if config.rate_limit.default_requests_per_second == 0 {
        errors.push("default_requests_per_second must be > 0".to_string());
    }

    if config.proxy.tls_cert.is_some() != config.proxy.tls_key.is_some() {
        errors.push("tls_cert and tls_key must both be set or both be unset".to_string());
    }
    if config.proxy.https_port > 0 && config.proxy.tls_cert.is_none() {
        errors.push("https_port requires tls_cert and tls_key".to_string());
    }

    if config.admin.tls_cert.is_some() != config.admin.tls_key.is_some() {
        errors
            .push("admin.tls_cert and admin.tls_key must both be set or both be unset".to_string());
    }

    validate_bind_address(
        "admin.bind_address",
        &config.admin.bind_address,
        &mut errors,
    );
    validate_bind_address(
        "admin.artifact_bind_address",
        &config.admin.artifact_bind_address,
        &mut errors,
    );
    validate_ip_literal(
        "runtime.instance_bind_address",
        &config.runtime.instance_bind_address,
        &mut errors,
    );
    match config.runtime.key_source.as_str() {
        "file" => {
            if config
                .runtime
                .key_file
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push("runtime.key_source = \"file\" requires runtime.key_file".to_string());
            }
        }
        "command" => {
            if config.runtime.key_command.is_empty()
                || config
                    .runtime
                    .key_command
                    .iter()
                    .any(|arg| arg.trim().is_empty())
            {
                errors.push(
                    "runtime.key_source = \"command\" requires runtime.key_command with non-empty argv entries"
                        .to_string(),
                );
            }
        }
        "vault-kv" => {
            if config
                .runtime
                .key_vault_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"vault-kv\" requires runtime.key_vault_url".to_string(),
                );
            } else if let Some(url) = config.runtime.key_vault_url.as_deref() {
                match Url::parse(url.trim()) {
                    Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
                    Ok(_) => errors.push("runtime.key_vault_url must use http or https".to_string()),
                    Err(err) => errors.push(format!(
                        "runtime.key_vault_url is not a valid URL: {}",
                        err
                    )),
                }
            }
            if config
                .runtime
                .key_vault_token_env
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"vault-kv\" requires runtime.key_vault_token_env"
                        .to_string(),
                );
            }
            if config
                .runtime
                .key_vault_path
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"vault-kv\" requires runtime.key_vault_path".to_string(),
                );
            }
            if config.runtime.key_vault_mount.trim().is_empty() {
                errors.push("runtime.key_vault_mount must not be empty".to_string());
            }
            if config.runtime.key_vault_field.trim().is_empty() {
                errors.push("runtime.key_vault_field must not be empty".to_string());
            }
        }
        "vault-transit" => {
            if config
                .runtime
                .key_vault_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"vault-transit\" requires runtime.key_vault_url"
                        .to_string(),
                );
            } else if let Some(url) = config.runtime.key_vault_url.as_deref() {
                match Url::parse(url.trim()) {
                    Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
                    Ok(_) => errors.push("runtime.key_vault_url must use http or https".to_string()),
                    Err(err) => errors.push(format!(
                        "runtime.key_vault_url is not a valid URL: {}",
                        err
                    )),
                }
            }
            if config
                .runtime
                .key_vault_token_env
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"vault-transit\" requires runtime.key_vault_token_env"
                        .to_string(),
                );
            }
            if config.runtime.key_vault_transit_mount.trim().is_empty() {
                errors.push("runtime.key_vault_transit_mount must not be empty".to_string());
            }
            if config
                .runtime
                .key_vault_transit_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"vault-transit\" requires runtime.key_vault_transit_key"
                        .to_string(),
                );
            }
            if config
                .runtime
                .key_vault_transit_context
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"vault-transit\" requires runtime.key_vault_transit_context"
                        .to_string(),
                );
            }
        }
        "aws-kms-hmac" => {
            if config
                .runtime
                .key_aws_kms_region
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"aws-kms-hmac\" requires runtime.key_aws_kms_region"
                        .to_string(),
                );
            }
            if let Some(endpoint) = config.runtime.key_aws_kms_endpoint.as_deref() {
                match Url::parse(endpoint.trim()) {
                    Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
                    Ok(_) => errors.push(
                        "runtime.key_aws_kms_endpoint must use http or https".to_string(),
                    ),
                    Err(err) => errors.push(format!(
                        "runtime.key_aws_kms_endpoint is not a valid URL: {}",
                        err
                    )),
                }
            }
            if config
                .runtime
                .key_aws_kms_key_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"aws-kms-hmac\" requires runtime.key_aws_kms_key_id"
                        .to_string(),
                );
            }
            if config
                .runtime
                .key_aws_kms_context
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                errors.push(
                    "runtime.key_source = \"aws-kms-hmac\" requires runtime.key_aws_kms_context"
                        .to_string(),
                );
            }
        }
        "generate" => {}
        spec if spec.starts_with("env:") || spec.starts_with("passphrase-env:") => {}
        other => errors.push(format!(
            "runtime.key_source '{}' is unsupported; expected generate, file, command, vault-kv, vault-transit, aws-kms-hmac, env:VAR_NAME, or passphrase-env:VAR_NAME",
            other
        )),
    }
    validate_admin_advertisement(config, &mut errors);

    if config.auth.enabled && config.auth.require_tls && !admin_tls_material_configured(config) {
        errors.push(
            "auth.require_tls = true requires either admin.tls_cert/admin.tls_key or proxy.tls_cert/proxy.tls_key"
                .to_string(),
        );
    }

    if config.auth.enabled
        && !config.auth.require_tls
        && !bind_address_is_loopback(&config.admin.bind_address)
        && config.auth.trusted_proxies.is_empty()
    {
        errors.push(
            "auth.require_tls = false with a non-loopback admin.bind_address requires auth.trusted_proxies so forwarded client IP headers are only trusted from explicit peers"
                .to_string(),
        );
    }

    if !bind_address_is_loopback(&config.admin.artifact_bind_address)
        && effective_write_auth_token(config).is_none()
    {
        errors.push(
            "non-loopback admin.artifact_bind_address requires auth.write_token or legacy admin.auth_token so remote artifact uploads/authorization are not exposed without authentication"
                .to_string(),
        );
    }

    if let Some(ref key_hex) = config.runtime.upgrade_signing_public_key {
        let trimmed = key_hex.trim();
        match hex::decode(trimmed) {
            Ok(bytes) if bytes.len() == 32 => {}
            Ok(bytes) => errors.push(format!(
                "runtime.upgrade_signing_public_key must decode to 32 bytes, got {} bytes",
                bytes.len()
            )),
            Err(e) => errors.push(format!(
                "runtime.upgrade_signing_public_key is not valid hex: {}",
                e
            )),
        }
    }

    if config.runtime.pooling_total_component_instances == 0 {
        errors.push("runtime.pooling_total_component_instances must be > 0".to_string());
    }
    if config.runtime.pooling_allocator {
        if let Some(v) = config.runtime.pooling_max_core_instances_per_component {
            if v == 0 {
                errors.push(
                    "runtime.pooling_max_core_instances_per_component must be > 0".to_string(),
                );
            }
        }
        if let Some(v) = config.runtime.pooling_max_memories_per_component {
            if v == 0 {
                errors.push("runtime.pooling_max_memories_per_component must be > 0".to_string());
            }
        }
        if let Some(v) = config.runtime.pooling_max_tables_per_component {
            if v == 0 {
                errors.push("runtime.pooling_max_tables_per_component must be > 0".to_string());
            }
        }
    }

    let auth_config: common::auth::AuthConfig = config.auth.clone().into();
    if let Err(e) = auth_config.validate() {
        errors.push(e);
    }

    if config.admin.auth_token.is_some() && config.auth.enabled && config.auth.write_token.is_some()
    {
        errors.push(
            "both admin.auth_token (legacy) and auth.write_token are set — \
             remove admin.auth_token and use the [auth] section instead"
                .to_string(),
        );
    }

    if !errors.is_empty() {
        return Err(PlatformError::ConfigValidation(format!(
            "configuration validation failed:\n  - {}",
            errors.join("\n  - ")
        )));
    }

    Ok(())
}

pub(crate) fn validate_hot_config(config: &crate::hot::HotConfig) -> Result<(), PlatformError> {
    let mut errors = Vec::new();

    let valid_levels = ["trace", "debug", "info", "warn", "error"];
    if !valid_levels.contains(&config.logging.level.as_str()) {
        errors.push(format!(
            "invalid log level '{}', must be one of: {}",
            config.logging.level,
            valid_levels.join(", ")
        ));
    }

    if config.gc.disk_warning_threshold <= 0.0 || config.gc.disk_warning_threshold > 1.0 {
        errors.push("disk_warning_threshold must be between 0.0 and 1.0".to_string());
    }
    if config.gc.artifact_keep_versions == 0 {
        errors.push("artifact_keep_versions must be > 0".to_string());
    }

    if config.ebpf.fd_soft_limit >= config.ebpf.fd_hard_limit {
        errors.push("fd_soft_limit must be less than fd_hard_limit".to_string());
    }
    if config.ebpf.mem_low_threshold_pages <= config.ebpf.mem_critical_threshold_pages {
        errors.push(
            "mem_low_threshold_pages must be greater than mem_critical_threshold_pages".to_string(),
        );
    }

    if config.health.check_interval_secs == 0 {
        errors.push("check_interval_secs must be > 0".to_string());
    }
    if config.health.default_fuel_quota == 0 {
        errors.push("default_fuel_quota must be > 0".to_string());
    }
    if config.health.default_memory_pages == 0 {
        errors.push("default_memory_pages must be > 0".to_string());
    }

    if config.rate_limit.default_requests_per_second == 0 {
        errors.push("default_requests_per_second must be > 0".to_string());
    }

    if !errors.is_empty() {
        return Err(PlatformError::ConfigValidation(format!(
            "hot configuration validation failed:\n  - {}",
            errors.join("\n  - ")
        )));
    }

    Ok(())
}
