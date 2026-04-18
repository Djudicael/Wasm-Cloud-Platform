// crates/storage/src/artifact.rs
use crate::{
    tables::{ARTIFACTS, ARTIFACT_HASHES, RAW_WASM},
    Store,
};
use common::{error::PlatformError, types::AppId};
use redb::{ReadableDatabase, ReadableTable};

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

    // ── Raw Wasm Storage ─────────────────────────────────────────────────────

    /// Save raw .wasm bytes (pre-compilation) keyed by SHA-256.
    pub fn save_raw_wasm(&self, sha256: &str, bytes: &[u8]) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(RAW_WASM)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert(sha256, bytes)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }

    /// Load raw .wasm bytes by SHA-256.
    pub fn load_raw_wasm(&self, sha256: &str) -> Result<Option<Vec<u8>>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(RAW_WASM)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        Ok(table
            .get(sha256)
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .map(|v| v.value().to_vec()))
    }

    /// Check if raw wasm exists without loading the bytes.
    pub fn raw_wasm_exists(&self, sha256: &str) -> Result<bool, PlatformError> {
        Ok(self.load_raw_wasm(sha256)?.is_some())
    }

    /// Delete raw wasm bytes.
    pub fn delete_raw_wasm(&self, sha256: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(RAW_WASM)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .remove(sha256)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }

    /// Prune all raw wasm bytes (garbage collection).
    /// Returns the number of entries deleted.
    pub fn prune_raw_wasm_older_than(&self, _hours: u64) -> Result<u64, PlatformError> {
        // For now: delete all raw wasm
        // Future: add timestamp tracking in a metadata table
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let raw_table = tx
            .open_table(RAW_WASM)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let sha256s: Vec<String> = raw_table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .filter_map(|e| e.ok())
            .map(|(k, _)| k.value().to_string())
            .collect();
        drop(raw_table);
        drop(tx);

        let mut deleted = 0u64;
        for sha256 in sha256s {
            self.delete_raw_wasm(&sha256)?;
            deleted += 1;
        }
        Ok(deleted)
    }

    // ── Artifact Hash Metadata ───────────────────────────────────────────────

    /// Save the SHA-256 hash associated with an app_id.
    pub fn save_artifact_hash(&self, app_id: &AppId, sha256: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(ARTIFACT_HASHES)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert(app_id.0.as_str(), sha256)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }

    /// Load the SHA-256 hash associated with an app_id.
    pub fn get_artifact_sha256(&self, app_id: &AppId) -> Result<Option<String>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(ARTIFACT_HASHES)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        Ok(table
            .get(app_id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .map(|v| v.value().to_string()))
    }

    /// Delete artifact hash metadata.
    pub fn delete_artifact_hash(&self, app_id: &AppId) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(ARTIFACT_HASHES)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .remove(app_id.0.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_raw_wasm_lifecycle() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = Store::open(temp_file.path()).unwrap();

        let wasm_bytes = b"fake wasm binary content";
        let sha256 = "abc123def456";

        // 1. Initially should not exist
        assert!(!store.raw_wasm_exists(sha256).unwrap());
        assert!(store.load_raw_wasm(sha256).unwrap().is_none());

        // 2. Save and verify exists
        store.save_raw_wasm(sha256, wasm_bytes).unwrap();
        assert!(store.raw_wasm_exists(sha256).unwrap());

        // 3. Load and verify content
        let loaded = store.load_raw_wasm(sha256).unwrap().unwrap();
        assert_eq!(loaded, wasm_bytes);

        // 4. Delete and verify gone
        store.delete_raw_wasm(sha256).unwrap();
        assert!(!store.raw_wasm_exists(sha256).unwrap());
        assert!(store.load_raw_wasm(sha256).unwrap().is_none());
    }

    #[test]
    fn test_raw_wasm_persistence() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        let wasm_bytes = b"persistent wasm data";
        let sha256 = "persistent123";

        {
            let store = Store::open(&path).unwrap();
            store.save_raw_wasm(sha256, wasm_bytes).unwrap();
        }

        // Reopen and verify data survived
        let store = Store::open(&path).unwrap();
        let loaded = store.load_raw_wasm(sha256).unwrap().unwrap();
        assert_eq!(loaded, wasm_bytes);
    }

    #[test]
    fn test_prune_raw_wasm() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = Store::open(temp_file.path()).unwrap();

        // Add multiple entries
        store.save_raw_wasm("sha1", b"data1").unwrap();
        store.save_raw_wasm("sha2", b"data2").unwrap();
        store.save_raw_wasm("sha3", b"data3").unwrap();

        // Prune all
        let deleted = store.prune_raw_wasm_older_than(24).unwrap();
        assert_eq!(deleted, 3);

        // Verify all gone
        assert!(!store.raw_wasm_exists("sha1").unwrap());
        assert!(!store.raw_wasm_exists("sha2").unwrap());
        assert!(!store.raw_wasm_exists("sha3").unwrap());
    }
}
