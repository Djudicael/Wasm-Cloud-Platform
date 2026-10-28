use common::{error::PlatformError, types::AppConfig};
use secrets::SecretProvider;

pub(crate) async fn resolve_declared(
    config: &AppConfig,
    provider: Option<&dyn SecretProvider>,
) -> Result<Vec<(String, String)>, PlatformError> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for key in &config.secret_keys {
        if key.is_empty()
            || key.len() > 256
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || crate::config_validator::RESERVED_VARS.contains(&key.as_str())
            || !seen.insert(key)
        {
            return Err(PlatformError::ConfigValidation(
                "invalid declared secret key".into(),
            ));
        }
        let provider = provider.ok_or_else(|| {
            PlatformError::ConfigValidation("declared secret provider unavailable".into())
        })?;
        let value = provider.get(&config.id, key).await.map_err(|_| {
            PlatformError::ConfigValidation(format!("declared secret unavailable: {key}"))
        })?;
        if value.contains('\0') {
            return Err(PlatformError::ConfigValidation(
                "secret cannot be an environment value".into(),
            ));
        }
        result.push((key.clone(), value));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn declared_values_are_scoped_ephemeral_and_refreshed_on_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let store = storage::Store::open(&directory.path().join("state.redb")).unwrap();
        let provider = secrets::LocalSecretProvider::new(
            store.clone(),
            secrets::crypto::SymmetricKey::generate(),
        );
        let mut config = AppConfig::default_for(common::types::AppId("agents/worker:v1".into()));
        config.secret_keys = vec!["MODEL_API_KEY".into()];
        provider
            .set(&config.id, "MODEL_API_KEY", "first-private-value")
            .await
            .unwrap();
        provider
            .set(&config.id, "DATABASE_URL", "must-not-be-injected")
            .await
            .unwrap();
        store.save_config(&config).unwrap();
        assert_eq!(
            resolve_declared(&config, Some(&provider)).await.unwrap(),
            vec![("MODEL_API_KEY".into(), "first-private-value".into())]
        );
        provider
            .set(&config.id, "MODEL_API_KEY", "rotated-private-value")
            .await
            .unwrap();
        assert_eq!(
            resolve_declared(&config, Some(&provider)).await.unwrap()[0].1,
            "rotated-private-value"
        );
        assert!(
            !serde_json::to_string(&store.load_config(&config.id).unwrap().unwrap())
                .unwrap()
                .contains("private-value")
        );
        config.id = common::types::AppId("agents/other:v1".into());
        assert!(resolve_declared(&config, Some(&provider)).await.is_err());
    }
    #[tokio::test]
    async fn missing_provider_and_reserved_keys_fail_closed() {
        let mut config = AppConfig::default_for(common::types::AppId("agents/worker:v1".into()));
        assert!(resolve_declared(&config, None).await.unwrap().is_empty());
        for key in ["MISSING", "PORT", "BAD=KEY", ""] {
            config.secret_keys = vec![key.into()];
            assert!(resolve_declared(&config, None).await.is_err());
        }
    }
}
