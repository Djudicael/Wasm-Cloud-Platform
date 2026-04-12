use common::{error::PlatformError, types::AppConfig};
use secrets::SecretProvider;
use storage::Store;

pub struct EnvResolver<S: SecretProvider> {
    store: Store,
    secret_provider: S,
}

impl<S: SecretProvider> EnvResolver<S> {
    pub fn new(store: Store, secret_provider: S) -> Self {
        EnvResolver {
            store,
            secret_provider,
        }
    }

    /// Resolve the full environment for an app at spawn time.
    /// Returns a flat Vec<(key, value)> ready for WasiEnv injection.
    pub async fn resolve(
        &self,
        config: &AppConfig,
        host_port: u16,
        instance_id: &str,
        node_id: &str,
    ) -> Result<Vec<(String, String)>, PlatformError> {
        let mut env: Vec<(String, String)> = Vec::new();

        // 1. Static vars from config (lowest priority)
        for (k, v) in &config.env_vars {
            env.push((k.clone(), v.clone()));
        }

        // 2. Resolved secrets (override static vars if key collides)
        for secret_key in &config.secret_keys {
            match self.secret_provider.get(&config.id, secret_key).await {
                Ok(value) => {
                    // Remove any duplicate from static vars
                    env.retain(|(k, _)| k != secret_key);
                    env.push((secret_key.clone(), value));
                }
                Err(e) => {
                    tracing::warn!(
                        app = %config.id.0,
                        key = secret_key,
                        error = %e,
                        "secret not found, skipping"
                    );
                }
            }
        }

        // 3. Platform-injected vars (highest priority, always override)
        // Remove prior keys to enforce priority overrides
        env.retain(|(k, _)| {
            k != "PORT" && k != "HOST_PORT" && k != "APP_ID" && k != "INSTANCE_ID" && k != "NODE_ID"
        });

        env.push(("PORT".to_string(), config.wasm_bind_port.to_string()));
        env.push(("HOST_PORT".to_string(), host_port.to_string()));
        env.push(("APP_ID".to_string(), config.id.0.clone()));
        env.push(("INSTANCE_ID".to_string(), instance_id.to_string()));
        env.push(("NODE_ID".to_string(), node_id.to_string()));

        Ok(env)
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
            if key == "MY_SECRET" {
                Ok("secret_value".to_string())
            } else {
                Err(PlatformError::Encryption("Not found".into()))
            }
        }
    }

    #[tokio::test]
    async fn test_port_always_present() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let store = Store::open(&db_path).unwrap();

        let resolver = EnvResolver::new(store, MockSecretProvider);

        let app_id = AppId::new("test", "v1");
        let mut config = AppConfig::default_for(app_id);
        config.wasm_bind_port = 8080;

        let env = resolver
            .resolve(&config, 9999, "inst-123", "node-xyz")
            .await
            .unwrap();

        let port_var = env.iter().find(|(k, _)| k == "PORT");
        assert!(port_var.is_some());
        assert_eq!(port_var.unwrap().1, "8080");

        let host_port_var = env.iter().find(|(k, _)| k == "HOST_PORT");
        assert!(host_port_var.is_some());
        assert_eq!(host_port_var.unwrap().1, "9999");
    }

    #[tokio::test]
    async fn test_secret_overrides_static_env() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test2.db");
        let store = Store::open(&db_path).unwrap();

        let resolver = EnvResolver::new(store, MockSecretProvider);

        let app_id = AppId::new("test", "v1");
        let mut config = AppConfig::default_for(app_id);

        // Static env var
        config
            .env_vars
            .insert("MY_SECRET".to_string(), "static_value".to_string());

        // Same key as a secret
        config.secret_keys.push("MY_SECRET".to_string());

        let env = resolver
            .resolve(&config, 9999, "inst-123", "node-xyz")
            .await
            .unwrap();

        let secret_var = env.iter().find(|(k, _)| k == "MY_SECRET");
        assert!(secret_var.is_some());
        assert_eq!(secret_var.unwrap().1, "secret_value"); // Overridden by mock
    }
}
