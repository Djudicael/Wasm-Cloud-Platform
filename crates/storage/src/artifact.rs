// crates/storage/src/artifact.rs
use crate::{tables::ARTIFACTS, Store};
use common::{error::PlatformError, types::AppId};

impl Store {
    /// Persist a compiled Wasm artifact.
    /// `bytes` is the serialized Wasmtime Engine Artifact.
    pub fn store_artifact(&self, id: &AppId, bytes: &[u8]) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(ARTIFACTS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert(id.0.as_str(), bytes)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        tracing::info!(app = %id.0, bytes = bytes.len(), "artifact stored");
        Ok(())
    }

    /// Load a compiled artifact. Returns None if not yet compiled.
    pub fn load_artifact(&self, id: &AppId) -> Result<Option<Vec<u8>>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(ARTIFACTS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let result = table
            .get(id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .map(|v| v.value().to_vec());
        Ok(result)
    }

    /// Check if an artifact exists without loading the bytes.
    pub fn artifact_exists(&self, id: &AppId) -> Result<bool, PlatformError> {
        Ok(self.load_artifact(id)?.is_some())
    }

    /// Delete an artifact (e.g. when an app is undeployed).
    pub fn delete_artifact(&self, id: &AppId) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(ARTIFACTS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .remove(id.0.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }
}
