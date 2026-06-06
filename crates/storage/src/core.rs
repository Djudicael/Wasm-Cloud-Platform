use crate::{tables, Store, CURRENT_SCHEMA_VERSION};
use common::error::PlatformError;
use redb::Database;
use std::path::Path;
use std::sync::Arc;

impl Store {
    /// Write a health probe key to verify that redb is writable.
    pub fn write_health_probe(&self) -> Result<(), PlatformError> {
        const HEALTH_PROBE_TABLE: redb::TableDefinition<&str, &str> =
            redb::TableDefinition::new("__health_probe");

        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage(format!("health probe write begin: {}", e)))?;

        {
            let mut table = write_txn
                .open_table(HEALTH_PROBE_TABLE)
                .map_err(|e| PlatformError::storage(format!("health probe open: {}", e)))?;
            table
                .insert("probe", chrono::Utc::now().to_rfc3339().as_str())
                .map_err(|e| PlatformError::storage(format!("health probe insert: {}", e)))?;
        }

        write_txn
            .commit()
            .map_err(|e| PlatformError::storage(format!("health probe commit: {}", e)))?;

        let delete_txn = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage(format!("health probe delete begin: {}", e)))?;

        {
            let mut table = delete_txn
                .open_table(HEALTH_PROBE_TABLE)
                .map_err(|e| PlatformError::storage(format!("health probe open: {}", e)))?;
            table
                .remove("probe")
                .map_err(|e| PlatformError::storage(format!("health probe remove: {}", e)))?;
        }

        delete_txn
            .commit()
            .map_err(|e| PlatformError::storage(format!("health probe delete commit: {}", e)))?;

        Ok(())
    }
}

pub(crate) fn open_store(path: &Path) -> Result<Store, redb::Error> {
    let needs_backup = if path.exists() {
        let temp_db = Database::create(path)?;
        let temp_store = Store {
            db: Arc::new(temp_db),
            db_path: path.to_path_buf(),
        };
        let current = temp_store.read_schema_version().unwrap_or(0);
        drop(temp_store);
        current < CURRENT_SCHEMA_VERSION
    } else {
        false
    };

    if needs_backup {
        let current_version = {
            let temp_db = Database::create(path)?;
            let temp_store = Store {
                db: Arc::new(temp_db),
                db_path: path.to_path_buf(),
            };
            let v = temp_store.read_schema_version().unwrap_or(0);
            drop(temp_store);
            v
        };
        let backup_path = path.with_extension(format!("redb.v{current_version}.bak"));
        if !backup_path.exists() {
            std::fs::copy(path, &backup_path).map_err(redb::Error::Io)?;
            tracing::warn!(
                backup = %backup_path.display(),
                from_version = current_version,
                to_version = CURRENT_SCHEMA_VERSION,
                "Created backup before migration"
            );
        }
    }

    let db = Database::create(path)?;
    let tx = db.begin_write()?;
    {
        tx.open_table(tables::SCHEMA_META)?;
        tx.open_table(tables::ARTIFACTS)?;
        tx.open_table(tables::CONFIGS)?;
        tx.open_table(tables::SECRETS)?;
        tx.open_table(tables::METRICS)?;
        tx.open_table(tables::ROUTES)?;
        tx.open_table(tables::RAW_WASM)?;
        tx.open_table(tables::ARTIFACT_HASHES)?;
        tx.open_table(tables::BILLING)?;
        tx.open_table(tables::KEK)?;
        tx.open_table(tables::GATEWAY_CONFIGS)?;
        tx.open_table(tables::API_KEYS)?;
        tx.open_table(tables::CLUSTER_NODES)?;
        tx.open_table(tables::ARTIFACT_VERIFICATIONS)?;
    }
    tx.commit()?;

    let store = Store {
        db: Arc::new(db),
        db_path: path.to_path_buf(),
    };
    store.run_migrations()?;
    Ok(store)
}
