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
    /// In production, the KEK should be encrypted with a passphrase before storing.
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
    /// Returns None if no KEK has been stored yet.
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
}
