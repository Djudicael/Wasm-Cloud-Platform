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

    /// Persist the Key Encryption Key (KEK) blob to the database.
    ///
    /// The blob is normally a sealed/encrypted KEK produced by the node's
    /// configured key source. Legacy tests may also stage a raw 32-byte KEK so
    /// startup can migrate it into a sealed-at-rest form.
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

    /// Load the persisted Key Encryption Key (KEK) blob from the database.
    ///
    /// Callers interpret the blob as either a modern sealed KEK or a legacy raw
    /// 32-byte KEK that still needs migration.
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
