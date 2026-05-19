// crates/storage/src/secrets.rs
use crate::{tables::KEK, tables::SECRETS, Store};
use common::{error::PlatformError, types::AppId};
use redb::ReadableDatabase;

impl Store {
    pub fn save_secrets(&self, id: &AppId, encrypted_blob: &[u8]) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(SECRETS)
                .map_err(PlatformError::storage_source)?;
            table
                .insert(id.0.as_str(), encrypted_blob)
                .map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    pub fn load_secrets(&self, id: &AppId) -> Result<Option<Vec<u8>>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(SECRETS)
            .map_err(PlatformError::storage_source)?;
        let result = table
            .get(id.0.as_str())
            .map_err(PlatformError::storage_source)?
            .map(|v| v.value().to_vec());
        Ok(result)
    }

    pub fn delete_secrets(&self, id: &AppId) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(SECRETS)
                .map_err(PlatformError::storage_source)?;
            table
                .remove(id.0.as_str())
                .map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    /// Persist the Key Encryption Key (KEK) to the database.
    ///
    /// Legacy compatibility only: normal node startup no longer relies on
    /// plaintext KEK persistence in redb. This method exists so older nodes or
    /// migration tests can still stage a KEK for one-time migration into an
    /// external key source (for example `runtime.key_source = "file"`).
    pub fn save_kek(&self, kek_bytes: &[u8]) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx.open_table(KEK).map_err(PlatformError::storage_source)?;
            table
                .insert("kek", kek_bytes)
                .map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    /// Load the Key Encryption Key (KEK) from the database.
    ///
    /// Legacy compatibility only: returns the old plaintext-on-disk KEK so the
    /// node can migrate it into an external key source. New deployments should
    /// not depend on this for normal operation.
    pub fn load_kek(&self) -> Result<Option<Vec<u8>>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx.open_table(KEK).map_err(PlatformError::storage_source)?;
        let result = table
            .get("kek")
            .map_err(PlatformError::storage_source)?
            .map(|v| v.value().to_vec());
        Ok(result)
    }

    /// Delete a persisted KEK from the database.
    pub fn delete_kek(&self) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx.open_table(KEK).map_err(PlatformError::storage_source)?;
            table.remove("kek").map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }
}
