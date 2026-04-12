// crates/storage/src/artifact.rs
use crate::{tables::ARTIFACTS, Store};
use common::{error::PlatformError, types::AppId};
use redb::ReadableTable;

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

    /// Enforce max N versions. Deletes oldest when exceeded.
    pub fn prune_old_versions(
        &self,
        app_name: &str,
        keep: usize,
        active_versions: &[&str],
    ) -> Result<(), PlatformError> {
        let prefix = format!("{app_name}:");
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(crate::tables::ARTIFACTS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut versions: Vec<String> = table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .filter_map(|e| e.ok())
            .filter(|(k, _)| k.value().starts_with(&prefix))
            .map(|(k, _)| k.value().to_string())
            .collect();

        versions.sort(); // Assumes version suffix is lexicographically ordered (v1, v2, v10...)
        let to_delete: Vec<_> = versions
            .into_iter()
            .rev()
            .skip(keep)
            .filter(|v| !active_versions.contains(&v.as_str()))
            .collect();

        drop(table);
        drop(tx);

        for key in to_delete {
            let id = AppId(key);
            self.delete_artifact(&id)?;
        }
        Ok(())
    }
}
