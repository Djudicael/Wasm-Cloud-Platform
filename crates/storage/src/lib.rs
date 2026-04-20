#![allow(clippy::result_large_err)]

use common::auth::AuthConfig;
use common::error::PlatformError;
use redb::{Database, ReadableDatabase};
use std::path::Path;
use std::sync::Arc;

pub mod artifact;
pub mod artifact_server;
pub mod billing;
pub mod config;
pub mod gc;
pub mod gc_metrics;
pub mod integrity;
pub mod metrics;
pub mod routes;
pub mod secrets;
pub mod tables;

#[cfg(test)]
mod tests;

/// Current schema version.
/// Increment this when adding a new migration.
///
/// Version History:
/// - 0: Fresh database (no schema version written yet)
/// - 1: Initial schema with artifacts, configs, secrets, metrics, routes, raw_wasm, schema_meta tables
/// - 2: Added db_max_connections field to AppConfig, added artifact_hashes table
/// - 3: Added rate_limit field to AppConfig
/// - 4: Added BILLING and KEK tables
const CURRENT_SCHEMA_VERSION: u32 = 4;

#[derive(Clone)]
pub struct Store {
    pub db: Arc<Database>,
    db_path: std::path::PathBuf,
}

impl Store {
    /// Open (or create) the database at the given path.
    /// Creates all table definitions on first run, then runs any pending migrations.
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        // First, check if we need to back up before opening
        // This must be done before creating the database to avoid file locks on Windows
        let needs_backup = if path.exists() {
            // Temporarily open just to check version
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

        // Create backup if needed (before reopening)
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

        // Ensure tables exist (idempotent)
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
        }
        tx.commit()?;

        let store = Store {
            db: Arc::new(db),
            db_path: path.to_path_buf(),
        };

        // Run migrations
        store.run_migrations()?;

        Ok(store)
    }

    /// Read the current schema version from the database.
    /// Returns 0 if no version has been written yet (fresh database).
    fn read_schema_version(&self) -> Result<u32, redb::Error> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(tables::SCHEMA_META)?;
        Ok(table
            .get("version")?
            .map(|v| v.value().parse::<u32>().unwrap_or(0))
            .unwrap_or(0))
    }

    /// Write the schema version to the database.
    fn write_schema_version(&self, version: u32) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META)?;
            table.insert("version", version.to_string().as_str())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Run all pending migrations to bring the database to the current schema version.
    fn run_migrations(&self) -> Result<(), redb::Error> {
        let current = self.read_schema_version()?;

        tracing::info!(
            current = current,
            target = CURRENT_SCHEMA_VERSION,
            "Checking schema version"
        );

        if current == CURRENT_SCHEMA_VERSION {
            return Ok(());
        }

        if current > CURRENT_SCHEMA_VERSION {
            panic!(
                "Database schema version {current} is NEWER than the binary supports ({CURRENT_SCHEMA_VERSION}). \
                 Downgrade is not supported. Use a newer binary or restore from backup."
            );
        }

        // Apply migrations in order
        for v in current..CURRENT_SCHEMA_VERSION {
            let target = v + 1;
            tracing::info!(from = v, to = target, "Running migration");

            // Apply migration and bump version in a single transaction
            self.apply_migration_transactional(target)?;

            tracing::info!(version = target, "Migration complete");
        }

        Ok(())
    }

    /// Apply a single migration in a transaction.
    /// The migration data changes and version bump happen atomically.
    /// If the migration fails, the version is NOT incremented.
    fn apply_migration_transactional(&self, target_version: u32) -> Result<(), redb::Error> {
        match target_version {
            1 => {
                // v0 → v1: Initial schema, all tables already created in open()
                // Just bump the version - no data migration needed
                tracing::info!("schema v1: initial schema created");
                self.write_schema_version(1)?;
            }
            2 => {
                // v1 → v2: Add db_max_connections field to all AppConfig records
                // This migration includes the version bump in the same transaction
                self.migrate_v1_to_v2()?;
            }
            3 => {
                // v2 → v3: Add rate_limit field to all AppConfig records
                // This migration includes the version bump in the same transaction
                self.migrate_v2_to_v3()?;
            }
            4 => {
                // v3 → v4: Ensure BILLING and KEK tables exist
                // These tables are created in open() for fresh databases,
                // but older databases at v3 may not have them.
                self.migrate_v3_to_v4()?;
            }
            n => panic!("Unknown migration target: {n}"),
        }
        Ok(())
    }

    /// Migration v1 → v2: Add db_max_connections field to all AppConfig records.
    /// This migration is idempotent: records that already have the field are not modified.
    /// The version bump is included in the same transaction for atomicity.
    fn migrate_v1_to_v2(&self) -> Result<(), redb::Error> {
        use crate::tables::CONFIGS;
        use redb::ReadableTable;

        let tx = self.db.begin_read()?;
        let table = tx.open_table(CONFIGS)?;

        // Read all existing records
        let records: Vec<(String, String)> = table
            .iter()?
            .filter_map(|e: Result<_, _>| e.ok())
            .map(|(k, v)| (k.value().to_string(), v.value().to_string()))
            .collect();
        drop(table);
        drop(tx);

        // Rewrite with new default field AND bump version in same transaction
        let write_tx = self.db.begin_write()?;
        {
            // Update all config records
            let mut table = write_tx.open_table(CONFIGS)?;
            for (key, json_str) in records {
                // Parse as generic JSON (schema-agnostic)
                if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    // Add the new field with a safe default if it doesn't exist
                    if val.get("db_max_connections").is_none() {
                        val["db_max_connections"] = serde_json::json!(10);
                    }
                    let new_json = serde_json::to_string(&val).expect("re-serialize failed");
                    table.insert(key.as_str(), new_json.as_str())?;
                }
            }
            drop(table);

            // Bump schema version in the same transaction
            let mut meta_table = write_tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "2")?;
        }
        write_tx.commit()?;

        tracing::info!("v1→v2: added db_max_connections to all app configs");
        Ok(())
    }

    /// Migration v2 → v3: Add rate_limit field to all AppConfig records.
    /// This migration is idempotent: records that already have the field are not modified.
    /// The version bump is included in the same transaction for atomicity.
    /// Migration v3 → v4: Ensure BILLING and KEK tables exist.
    /// Older databases at schema v3 may not have these tables.
    /// The tables are created in open() for fresh databases, but this
    /// migration ensures they exist for upgraded databases.
    fn migrate_v3_to_v4(&self) -> Result<(), redb::Error> {
        use crate::tables::{BILLING, KEK};

        let tx = self.db.begin_write()?;
        {
            // Ensure BILLING table exists (redb creates on open_table if missing)
            let _ = tx.open_table(BILLING)?;
            // Ensure KEK table exists
            let _ = tx.open_table(KEK)?;

            // Bump schema version in the same transaction
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "4")?;
        }
        tx.commit()?;

        tracing::info!("v3→v4: ensured BILLING and KEK tables exist");
        Ok(())
    }

    fn migrate_v2_to_v3(&self) -> Result<(), redb::Error> {
        use crate::tables::CONFIGS;
        use redb::ReadableTable;

        let tx = self.db.begin_read()?;
        let table = tx.open_table(CONFIGS)?;

        // Read all existing records
        let records: Vec<(String, String)> = table
            .iter()?
            .filter_map(|e: Result<_, _>| e.ok())
            .map(|(k, v)| (k.value().to_string(), v.value().to_string()))
            .collect();
        drop(table);
        drop(tx);

        // Rewrite with new default field AND bump version in same transaction
        let write_tx = self.db.begin_write()?;
        {
            // Update all config records
            let mut table = write_tx.open_table(CONFIGS)?;
            for (key, json_str) in records {
                // Parse as generic JSON (schema-agnostic)
                if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    // Add the new field with default if it doesn't exist
                    if val.get("rate_limit").is_none() {
                        val["rate_limit"] = serde_json::json!({
                            "requests_per_second": 1000,
                            "burst_capacity": 50,
                            "per_ip_limit": 100
                        });
                    }
                    let new_json = serde_json::to_string(&val).expect("re-serialize failed");
                    table.insert(key.as_str(), new_json.as_str())?;
                }
            }
            drop(table);

            // Bump schema version in the same transaction
            let mut meta_table = write_tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "3")?;
        }
        write_tx.commit()?;

        tracing::info!("v2→v3: added rate_limit to all app configs");
        Ok(())
    }
    /// Save a metadata key-value pair in the SCHEMA_META table.
    pub fn save_meta(&self, key: &str, value: &str) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META)?;
            table.insert(key, value)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Load a metadata value by key from the SCHEMA_META table.
    pub fn load_meta(&self, key: &str) -> Result<Option<String>, redb::Error> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(tables::SCHEMA_META)?;
        Ok(table.get(key)?.map(|v| v.value().to_string()))
    }

    /// Delete a metadata key from the SCHEMA_META table.
    pub fn delete_meta(&self, key: &str) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META)?;
            table.remove(key)?;
        }
        tx.commit()?;
        Ok(())
    }

    // ── Auth Config Persistence ────────────────────────────────────────────────

    /// Key used to store auth config overrides in the SCHEMA_META table.
    ///
    /// When auth tokens are rotated via the admin API, the updated config is
    /// persisted here so it survives node restarts. On startup, the persisted
    /// config takes precedence over the TOML file values.
    const AUTH_CONFIG_KEY: &'static str = "auth_config_override";

    /// Save the auth configuration to the database.
    ///
    /// This is called after token rotation to persist the new tokens.
    /// The config is stored as JSON in the SCHEMA_META table.
    pub fn save_auth_config(&self, config: &AuthConfig) -> Result<(), PlatformError> {
        let json = serde_json::to_string(config)
            .map_err(|e| PlatformError::storage_with_msg("failed to serialize auth config", e))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META).map_err(|e| {
                PlatformError::storage_with_msg("failed to open SCHEMA_META table", e)
            })?;
            table
                .insert(Self::AUTH_CONFIG_KEY, json.as_str())
                .map_err(|e| PlatformError::storage_with_msg("failed to write auth config", e))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::storage_with_msg("failed to commit auth config", e))?;
        Ok(())
    }

    /// Load the persisted auth configuration from the database.
    ///
    /// Returns `Ok(Some(config))` if a persisted config exists,
    /// `Ok(None)` if no override has been saved (use TOML file values),
    /// or an error if the stored config is corrupt.
    pub fn load_auth_config(&self) -> Result<Option<AuthConfig>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx
            .open_table(tables::SCHEMA_META)
            .map_err(|e| PlatformError::storage_with_msg("failed to open SCHEMA_META table", e))?;
        match table
            .get(Self::AUTH_CONFIG_KEY)
            .map_err(|e| PlatformError::storage_with_msg("failed to read auth config", e))?
        {
            Some(v) => {
                let config: AuthConfig = serde_json::from_str(v.value()).map_err(|e| {
                    PlatformError::storage_with_msg(
                        "failed to deserialize persisted auth config — falling back to TOML file",
                        e,
                    )
                })?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Delete the persisted auth configuration from the database.
    ///
    /// This is used when resetting auth config to TOML file defaults.
    pub fn delete_auth_config(&self) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META).map_err(|e| {
                PlatformError::storage_with_msg("failed to open SCHEMA_META table", e)
            })?;
            table
                .remove(Self::AUTH_CONFIG_KEY)
                .map_err(|e| PlatformError::storage_with_msg("failed to delete auth config", e))?;
        }
        tx.commit().map_err(|e| {
            PlatformError::storage_with_msg("failed to commit auth config deletion", e)
        })?;
        Ok(())
    }
}
