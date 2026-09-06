use common::{error::PlatformError, types::AppConfig};
use secrets::SecretProvider;

#[derive(Debug)]
pub struct ConfigValidationError {
    pub missing_secrets: Vec<String>,
    pub reserved_conflicts: Vec<String>,
    pub invalid_fields: Vec<String>,
}

impl std::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.missing_secrets.is_empty() {
            writeln!(f, "missing secrets: {}", self.missing_secrets.join(", "))?;
        }
        if !self.reserved_conflicts.is_empty() {
            writeln!(
                f,
                "env_vars conflict with reserved names: {}",
                self.reserved_conflicts.join(", ")
            )?;
        }
        if !self.invalid_fields.is_empty() {
            for msg in &self.invalid_fields {
                writeln!(f, "invalid config: {msg}")?;
            }
        }
        Ok(())
    }
}

/// Reserved env var names that the platform always injects.
/// Apps must not set these in env_vars or secret_keys.
const RESERVED_VARS: &[&str] = &["PORT", "HOST_PORT", "APP_ID", "INSTANCE_ID", "NODE_ID"];

/// Validate the architecture-level placement contract without consulting an
/// external secret provider. Event consumers call this too, so a direct NATS
/// deployment cannot bypass the node-local mesh invariants enforced by ctl.
pub fn validate_placement_contract(config: &AppConfig) -> Result<(), PlatformError> {
    let mut invalid_fields = Vec::new();
    let mut dependencies = std::collections::HashSet::new();
    for dependency in &config.local_dependencies {
        if dependency == &config.id {
            invalid_fields
                .push("local dependency cannot refer to the application itself".to_string());
        }
        if dependency.namespace() != config.namespace {
            invalid_fields.push(format!(
                "local dependency {} must be in namespace {}",
                dependency.0, config.namespace
            ));
        }
        if !dependencies.insert(dependency.0.clone()) {
            invalid_fields.push(format!(
                "local dependency {} is declared more than once",
                dependency.0
            ));
        }
    }

    if invalid_fields.is_empty() {
        Ok(())
    } else {
        Err(PlatformError::ConfigValidation(
            invalid_fields
                .into_iter()
                .map(|message| format!("invalid config: {message}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

/// Validate an AppConfig before accepting a deployment.
/// Returns Ok(()) if the config is valid; Err with a detailed breakdown otherwise.
pub async fn validate_config<S: SecretProvider>(
    config: &AppConfig,
    secret_provider: &S,
) -> Result<(), PlatformError> {
    let mut error = ConfigValidationError {
        missing_secrets: Vec::new(),
        reserved_conflicts: Vec::new(),
        invalid_fields: Vec::new(),
    };

    // 1. Check that all declared secret_keys actually exist in the secrets store.
    for key in &config.secret_keys {
        if secret_provider.get(&config.id, key).await.is_err() {
            error.missing_secrets.push(key.clone());
        }
    }

    // 2. Check that env_vars do not collide with reserved platform variables.
    for key in config.env_vars.keys() {
        if RESERVED_VARS.contains(&key.as_str()) {
            error.reserved_conflicts.push(key.clone());
        }
    }

    // Also check secret_keys for reserved name collisions.
    for key in &config.secret_keys {
        if RESERVED_VARS.contains(&key.as_str()) {
            error.reserved_conflicts.push(key.clone());
        }
    }

    // 3. Sanity-check numeric fields.
    if config.max_instances == 0 {
        error
            .invalid_fields
            .push("max_instances must be > 0".to_string());
    }
    if config.idle_timeout_secs == 0 {
        error
            .invalid_fields
            .push("idle_timeout_secs must be > 0".to_string());
    }
    if config.wasm_bind_port == 0 {
        error
            .invalid_fields
            .push("wasm_bind_port must be > 0".to_string());
    }
    if config.fuel_quota.0 == 0 {
        error
            .invalid_fields
            .push("fuel_quota must be > 0".to_string());
    }
    if config.memory_limit.0 == 0 {
        error
            .invalid_fields
            .push("memory_limit must be > 0".to_string());
    }

    if let Err(PlatformError::ConfigValidation(messages)) = validate_placement_contract(config) {
        error.invalid_fields.extend(messages.lines().map(|message| {
            message
                .strip_prefix("invalid config: ")
                .unwrap_or(message)
                .to_string()
        }));
    }

    // 4. Fail if any issues were found.
    let has_errors = !error.missing_secrets.is_empty()
        || !error.reserved_conflicts.is_empty()
        || !error.invalid_fields.is_empty();

    if has_errors {
        Err(PlatformError::ConfigValidation(error.to_string()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use common::types::AppId;

    struct MockSecretProvider;

    #[async_trait]
    impl SecretProvider for MockSecretProvider {
        async fn set(
            &self,
            _app_id: &AppId,
            _key: &str,
            _value: &str,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
        async fn delete(&self, _app_id: &AppId, _key: &str) -> Result<(), PlatformError> {
            Ok(())
        }
        async fn list_keys(&self, _app_id: &AppId) -> Result<Vec<String>, PlatformError> {
            Ok(vec![])
        }
        async fn get(&self, _app_id: &AppId, key: &str) -> Result<String, PlatformError> {
            if key == "EXISTING_SECRET" {
                Ok("secret_value".to_string())
            } else {
                Err(PlatformError::encryption("Not found"))
            }
        }
    }

    #[tokio::test]
    async fn test_validate_config_all_errors() {
        let app_id = AppId::new("test", "v1");
        let mut config = AppConfig::default_for(app_id);

        // 1. Missing secret
        config.secret_keys.push("MISSING_SECRET".to_string());

        // 2. Reserved conflict
        config
            .env_vars
            .insert("PORT".to_string(), "1234".to_string());

        // 3. Invalid fields
        config.max_instances = 0;
        config.wasm_bind_port = 0;

        let result = validate_config(&config, &MockSecretProvider).await;

        assert!(result.is_err());
        if let Err(PlatformError::ConfigValidation(msg)) = result {
            assert!(msg.contains("missing secrets: MISSING_SECRET"));
            assert!(msg.contains("env_vars conflict with reserved names: PORT"));
            assert!(msg.contains("invalid config: max_instances must be > 0"));
            assert!(msg.contains("invalid config: wasm_bind_port must be > 0"));
        } else {
            panic!("Expected ConfigValidation error");
        }
    }

    #[tokio::test]
    async fn test_validate_config_success() {
        let app_id = AppId::new("test", "v1");
        let mut config = AppConfig::default_for(app_id);

        // Valid secret
        config.secret_keys.push("EXISTING_SECRET".to_string());
        // Valid env var
        config
            .env_vars
            .insert("CUSTOM_VAR".to_string(), "value".to_string());

        let result = validate_config(&config, &MockSecretProvider).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_validate_config_rejects_invalid_local_dependency_placement() {
        let app_id = AppId::new_namespaced("production", "caller", "v1");
        let mut config = AppConfig::default_for(app_id.clone());
        config.namespace = "production".to_string();
        config.local_dependencies = vec![
            app_id,
            AppId::new_namespaced("other", "database", "v1"),
            AppId::new_namespaced("production", "cache", "v1"),
            AppId::new_namespaced("production", "cache", "v1"),
        ];

        let error = validate_config(&config, &MockSecretProvider)
            .await
            .expect_err("invalid dependency placement must be rejected")
            .to_string();
        assert!(error.contains("cannot refer to the application itself"));
        assert!(error.contains("must be in namespace production"));
        assert!(error.contains("declared more than once"));
    }
}
