use crate::{
    crypto::{decrypt, encrypt, AppSecretBundle, EncryptedBlob, SymmetricKey},
    provider::SecretProvider,
};
use async_trait::async_trait;
use common::{error::PlatformError, types::AppId};
use redb::ReadableDatabase;
use std::sync::Arc;
use storage::{tables::SECRETS, Store};
use tokio::sync::RwLock;

pub struct LocalSecretProvider {
    store: Store,
    kek: Arc<SymmetricKey>,
    /// In-memory cache of decrypted DEKs (keyed by app_id).
    dek_cache: Arc<RwLock<std::collections::HashMap<String, Arc<SymmetricKey>>>>,
}

impl LocalSecretProvider {
    pub fn new(store: Store, kek: SymmetricKey) -> Self {
        LocalSecretProvider {
            store,
            kek: Arc::new(kek),
            dek_cache: Default::default(),
        }
    }

    /// Get or create the DEK for an app.
    ///
    /// Uses a double-check locking pattern to avoid TOCTOU races:
    /// 1. Fast path: read lock to check cache
    /// 2. Slow path: write lock, re-check cache, then load/create DEK
    async fn get_or_create_dek(&self, app_id: &AppId) -> Result<Arc<SymmetricKey>, PlatformError> {
        // Check in-memory cache first (read lock — fast path)
        {
            let cache = self.dek_cache.read().await;
            if let Some(key) = cache.get(&app_id.0) {
                return Ok(Arc::clone(key));
            }
        }

        // Not in cache — acquire write lock for the full operation
        let mut cache = self.dek_cache.write().await;

        // Double-check after acquiring write lock (another task may have inserted)
        if let Some(key) = cache.get(&app_id.0) {
            return Ok(Arc::clone(key));
        }

        // Load bundle from redb
        let dek = match self.load_bundle(app_id)? {
            Some(b) => {
                // Decrypt the DEK using the KEK
                let decrypted = decrypt(&self.kek, &EncryptedBlob(b.encrypted_dek))?;
                if decrypted.len() != 32 {
                    return Err(PlatformError::encryption(format!(
                        "decrypted DEK for app '{}' had invalid length {}",
                        app_id.0,
                        decrypted.len()
                    )));
                }
                let mut key_bytes = [0u8; 32];
                key_bytes.copy_from_slice(&decrypted);
                Arc::new(SymmetricKey::from_bytes(key_bytes))
            }
            None => {
                // First time: generate a new DEK, encrypt it with KEK, store bundle
                let new_dek = Arc::new(SymmetricKey::generate());
                let encrypted_dek = encrypt(&self.kek, new_dek.as_bytes())?;
                let bundle = AppSecretBundle {
                    app_id: app_id.0.clone(),
                    encrypted_dek: encrypted_dek.0,
                    secrets: Default::default(),
                    version: 1,
                    updated_at: now_secs(),
                };
                self.save_bundle(&bundle)?;
                new_dek
            }
        };

        // Cache the DEK (still holding write lock)
        cache.insert(app_id.0.clone(), Arc::clone(&dek));

        Ok(dek)
    }

    fn load_bundle(&self, app_id: &AppId) -> Result<Option<AppSecretBundle>, PlatformError> {
        let tx = self
            .store
            .db()
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(SECRETS)
            .map_err(PlatformError::storage_source)?;
        match table
            .get(app_id.0.as_str())
            .map_err(PlatformError::storage_source)?
        {
            Some(v) => {
                let bundle: AppSecretBundle =
                    bincode::deserialize(v.value()).map_err(PlatformError::storage_source)?;
                Ok(Some(bundle))
            }
            None => Ok(None),
        }
    }

    fn save_bundle(&self, bundle: &AppSecretBundle) -> Result<(), PlatformError> {
        let bytes = bincode::serialize(bundle).map_err(PlatformError::storage_source)?;
        let tx = self
            .store
            .db()
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(SECRETS)
                .map_err(PlatformError::storage_source)?;
            table
                .insert(bundle.app_id.as_str(), bytes.as_slice())
                .map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    /// Save bundle with optimistic concurrency check.
    ///
    /// Reads the current version from disk and compares it to `expected_version`.
    /// If they differ, another writer modified the bundle concurrently and this
    /// write is rejected to prevent lost updates.
    fn save_bundle_with_version(
        &self,
        app_id: &AppId,
        bundle: &AppSecretBundle,
        expected_version: u64,
    ) -> Result<(), PlatformError> {
        let current = self.load_bundle(app_id)?;
        match current {
            Some(existing) if existing.version != expected_version => {
                return Err(PlatformError::storage(format!(
                    "concurrent modification: expected version {}, found {}",
                    expected_version, existing.version
                )));
            }
            None => {
                return Err(PlatformError::storage("Bundle vanished before save"));
            }
            _ => {}
        }
        self.save_bundle(bundle)
    }
}

#[async_trait]
impl SecretProvider for LocalSecretProvider {
    async fn get(&self, app_id: &AppId, key: &str) -> Result<String, PlatformError> {
        let dek = self.get_or_create_dek(app_id).await?;
        let bundle = self
            .load_bundle(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;
        let encrypted_value = bundle.secrets.get(key).ok_or_else(|| {
            PlatformError::storage(format!("secret '{}' not found for app '{}'", key, app_id.0))
        })?;
        let plaintext = decrypt(&dek, &EncryptedBlob(encrypted_value.clone()))?;
        String::from_utf8(plaintext).map_err(|e| PlatformError::encryption(e.to_string()))
    }

    async fn set(&self, app_id: &AppId, key: &str, value: &str) -> Result<(), PlatformError> {
        let dek = self.get_or_create_dek(app_id).await?;
        let encrypted_value = encrypt(&dek, value.as_bytes())?;
        let mut bundle = self
            .load_bundle(app_id)?
            .ok_or_else(|| PlatformError::storage("Bundle vanished"))?;

        let expected_version = bundle.version;

        bundle.secrets.insert(key.to_string(), encrypted_value.0);
        bundle.version += 1;
        bundle.updated_at = now_secs();

        // Save with version check to prevent lost-update races
        self.save_bundle_with_version(app_id, &bundle, expected_version)?;

        Ok(())
    }

    async fn delete(&self, app_id: &AppId, key: &str) -> Result<(), PlatformError> {
        let mut bundle = self
            .load_bundle(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

        // If the key doesn't exist, there's nothing to delete — don't increment version
        if bundle.secrets.remove(key).is_none() {
            return Ok(());
        }

        let expected_version = bundle.version;
        bundle.version += 1;
        bundle.updated_at = now_secs();

        // Save with version check to prevent lost-update races
        self.save_bundle_with_version(app_id, &bundle, expected_version)?;

        Ok(())
    }

    async fn list_keys(&self, app_id: &AppId) -> Result<Vec<String>, PlatformError> {
        Ok(self
            .load_bundle(app_id)?
            .map(|b| b.secrets.keys().cloned().collect())
            .unwrap_or_default())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::types::AppId;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_store() -> Store {
        let idx = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("secrets_test_db_{}", idx));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);
        Store::open(&path).unwrap()
    }

    #[tokio::test]
    async fn test_local_provider_lifecycle() {
        let store = temp_store();
        let kek = SymmetricKey::generate();
        let provider = LocalSecretProvider::new(store, kek);

        let app_id = AppId("app1".into());

        // 1. Unknown key -> err
        let res = provider.get(&app_id, "DB_PASS").await;
        assert!(res.is_err());

        // 2. Set -> OK
        provider
            .set(&app_id, "DB_PASS", "super_secret")
            .await
            .unwrap();

        // 3. Get -> OK
        let val = provider.get(&app_id, "DB_PASS").await.unwrap();
        assert_eq!(val, "super_secret");

        // 4. List -> OK
        let keys = provider.list_keys(&app_id).await.unwrap();
        assert_eq!(keys, vec!["DB_PASS".to_string()]);

        // 5. Delete -> OK
        provider.delete(&app_id, "DB_PASS").await.unwrap();
        let res2 = provider.get(&app_id, "DB_PASS").await;
        assert!(res2.is_err());

        // 6. Delete non-existent key -> OK (no-op)
        provider.delete(&app_id, "NONEXISTENT").await.unwrap();
    }

    #[tokio::test]
    async fn test_local_provider_survives_restart_with_same_kek() {
        let store = temp_store();
        let kek_bytes = *SymmetricKey::generate().as_bytes();
        let app_id = AppId("app-restart:v1".into());

        {
            let provider =
                LocalSecretProvider::new(store.clone(), SymmetricKey::from_bytes(kek_bytes));
            provider
                .set(&app_id, "API_TOKEN", "secret-after-restart")
                .await
                .unwrap();
        }

        let restarted =
            LocalSecretProvider::new(store.clone(), SymmetricKey::from_bytes(kek_bytes));
        let plaintext = restarted.get(&app_id, "API_TOKEN").await.unwrap();
        assert_eq!(plaintext, "secret-after-restart");
    }

    #[tokio::test]
    async fn test_isolation() {
        let store = temp_store();
        let kek = SymmetricKey::generate();
        let provider = LocalSecretProvider::new(store, kek);

        let app_a = AppId("appA".into());
        let app_b = AppId("appB".into());

        provider.set(&app_a, "SECRET", "valueA").await.unwrap();
        provider.set(&app_b, "SECRET", "valueB").await.unwrap();

        let val_a = provider.get(&app_a, "SECRET").await.unwrap();
        let val_b = provider.get(&app_b, "SECRET").await.unwrap();

        assert_eq!(val_a, "valueA");
        assert_eq!(val_b, "valueB");
    }
}
