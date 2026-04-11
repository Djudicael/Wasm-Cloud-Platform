// crates/storage/src/secrets.rs
use crate::{tables::SECRETS, Store};
use common::{error::PlatformError, types::AppId};

impl Store {
    pub fn save_secrets(&self, id: &AppId, encrypted_blob: &[u8]) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(SECRETS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert(id.0.as_str(), encrypted_blob)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn load_secrets(&self, id: &AppId) -> Result<Option<Vec<u8>>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(SECRETS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let result = table
            .get(id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .map(|v| v.value().to_vec());
        Ok(result)
    }

    pub fn delete_secrets(&self, id: &AppId) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(SECRETS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .remove(id.0.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }
}
