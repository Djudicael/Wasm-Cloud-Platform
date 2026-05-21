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
pub mod health;
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
/// - 5: Added GATEWAY_CONFIGS table
/// - 6: Added API_KEYS table
/// - 7: Added CLUSTER_NODES table
const CURRENT_SCHEMA_VERSION: u32 = 7;

#[derive(Clone)]
pub struct Store {
    pub(crate) db: Arc<Database>,
    db_path: std::path::PathBuf,
}

impl Store {
    /// Get a reference to the underlying database.
    /// This is exposed for crates that need direct redb access (e.g., secrets).
    pub fn db(&self) -> &Arc<Database> {
        &self.db
    }

    /// Write a health probe key to verify that redb is writable.
    /// This writes a small record and immediately deletes it.
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

        // Clean up the probe key
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
            tx.open_table(tables::GATEWAY_CONFIGS)?;
            tx.open_table(tables::API_KEYS)?;
            tx.open_table(tables::CLUSTER_NODES)?;
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
            return Err(redb::Error::Corrupted(format!(
                "Database schema version {current} is NEWER than the binary supports ({CURRENT_SCHEMA_VERSION}). \
                 Downgrade is not supported. Use a newer binary or restore from backup."
            )));
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
            5 => {
                // v4 → v5: Ensure GATEWAY_CONFIGS table exists
                self.migrate_v4_to_v5()?;
            }
            6 => {
                // v5 → v6: Ensure API_KEYS table exists
                self.migrate_v5_to_v6()?;
            }
            7 => {
                // v6 → v7: Ensure CLUSTER_NODES table exists
                self.migrate_v6_to_v7()?;
            }
            n => {
                return Err(redb::Error::Corrupted(format!(
                    "Unknown migration target: {n}"
                )))
            }
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
            .filter_map(|e: Result<_, _>| match e {
                Ok(v) => Some(v),
                Err(err) => {
                    tracing::warn!(error = %err, "error iterating configs table entry during v1→v2 migration, skipping");
                    None
                }
            })
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
                    let new_json = serde_json::to_string(&val)
                        .map_err(|e| redb::Error::Corrupted(format!("re-serialize failed: {e}")))?;
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
            .filter_map(|e: Result<_, _>| match e {
                Ok(v) => Some(v),
                Err(err) => {
                    tracing::warn!(error = %err, "error iterating configs table entry during v2→v3 migration, skipping");
                    None
                }
            })
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
                    let new_json = serde_json::to_string(&val)
                        .map_err(|e| redb::Error::Corrupted(format!("re-serialize failed: {e}")))?;
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

    /// Migration v4 → v5: Ensure GATEWAY_CONFIGS table exists.
    fn migrate_v4_to_v5(&self) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let _ = tx.open_table(tables::GATEWAY_CONFIGS)?;
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "5")?;
        }
        tx.commit()?;
        tracing::info!("v4→v5: ensured GATEWAY_CONFIGS table exists");
        Ok(())
    }

    /// Migration v5 → v6: Ensure API_KEYS table exists.
    fn migrate_v5_to_v6(&self) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let _ = tx.open_table(tables::API_KEYS)?;
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "6")?;
        }
        tx.commit()?;
        tracing::info!("v5→v6: ensured API_KEYS table exists");
        Ok(())
    }

    /// Migration v6 → v7: Ensure CLUSTER_NODES table exists.
    fn migrate_v6_to_v7(&self) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let _ = tx.open_table(tables::CLUSTER_NODES)?;
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "7")?;
        }
        tx.commit()?;
        tracing::info!("v6→v7: ensured CLUSTER_NODES table exists");
        Ok(())
    }

    // ── Cluster Node Registry ────────────────────────────────────────────────

    pub fn save_cluster_node(
        &self,
        node: &common::types::ClusterNodeRecord,
    ) -> Result<(), PlatformError> {
        let json = serde_json::to_string(node)
            .map_err(|e| PlatformError::storage_with_msg("failed to serialize cluster node", e))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::CLUSTER_NODES).map_err(|e| {
                PlatformError::storage_with_msg("failed to open CLUSTER_NODES table", e)
            })?;
            table
                .insert(node.node_id.as_str(), json.as_str())
                .map_err(|e| PlatformError::storage_with_msg("failed to write cluster node", e))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::storage_with_msg("failed to commit cluster node", e))?;
        Ok(())
    }

    pub fn load_cluster_node(
        &self,
        node_id: &str,
    ) -> Result<Option<common::types::ClusterNodeRecord>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx.open_table(tables::CLUSTER_NODES).map_err(|e| {
            PlatformError::storage_with_msg("failed to open CLUSTER_NODES table", e)
        })?;
        match table
            .get(node_id)
            .map_err(|e| PlatformError::storage_with_msg("failed to read cluster node", e))?
        {
            Some(value) => {
                let node = serde_json::from_str(value.value()).map_err(|e| {
                    PlatformError::storage_with_msg("failed to deserialize cluster node", e)
                })?;
                Ok(Some(node))
            }
            None => Ok(None),
        }
    }

    pub fn list_cluster_nodes(
        &self,
    ) -> Result<Vec<common::types::ClusterNodeRecord>, PlatformError> {
        use redb::ReadableTable;

        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx.open_table(tables::CLUSTER_NODES).map_err(|e| {
            PlatformError::storage_with_msg("failed to open CLUSTER_NODES table", e)
        })?;
        let mut nodes = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| PlatformError::storage_with_msg("failed to iterate cluster nodes", e))?
        {
            let (_key, value) = entry.map_err(|e| {
                PlatformError::storage_with_msg("failed to read cluster node entry", e)
            })?;
            let node: common::types::ClusterNodeRecord = serde_json::from_str(value.value())
                .map_err(|e| {
                    PlatformError::storage_with_msg("failed to deserialize cluster node", e)
                })?;
            nodes.push(node);
        }
        nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(nodes)
    }

    // ── Gateway Config Persistence ─────────────────────────────────────────────

    /// Save a gateway route config for an app.
    pub fn save_gateway_config(
        &self,
        app_id: &str,
        config: &common::types::GatewayRouteConfig,
    ) -> Result<(), PlatformError> {
        let json = serde_json::to_string(config).map_err(|e| {
            PlatformError::storage_with_msg("failed to serialize gateway config", e)
        })?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::GATEWAY_CONFIGS).map_err(|e| {
                PlatformError::storage_with_msg("failed to open GATEWAY_CONFIGS table", e)
            })?;
            table.insert(app_id, json.as_str()).map_err(|e| {
                PlatformError::storage_with_msg("failed to write gateway config", e)
            })?;
        }
        tx.commit()
            .map_err(|e| PlatformError::storage_with_msg("failed to commit gateway config", e))?;
        Ok(())
    }

    /// Load a gateway route config for an app.
    pub fn load_gateway_config(
        &self,
        app_id: &str,
    ) -> Result<Option<common::types::GatewayRouteConfig>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx.open_table(tables::GATEWAY_CONFIGS).map_err(|e| {
            PlatformError::storage_with_msg("failed to open GATEWAY_CONFIGS table", e)
        })?;
        match table
            .get(app_id)
            .map_err(|e| PlatformError::storage_with_msg("failed to read gateway config", e))?
        {
            Some(v) => {
                let config: common::types::GatewayRouteConfig = serde_json::from_str(v.value())
                    .map_err(|e| {
                        PlatformError::storage_with_msg("failed to deserialize gateway config", e)
                    })?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Delete a gateway route config for an app.
    pub fn delete_gateway_config(&self, app_id: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::GATEWAY_CONFIGS).map_err(|e| {
                PlatformError::storage_with_msg("failed to open GATEWAY_CONFIGS table", e)
            })?;
            table.remove(app_id).map_err(|e| {
                PlatformError::storage_with_msg("failed to delete gateway config", e)
            })?;
        }
        tx.commit().map_err(|e| {
            PlatformError::storage_with_msg("failed to commit gateway config deletion", e)
        })?;
        Ok(())
    }

    /// List all gateway route configs.
    pub fn list_gateway_configs(
        &self,
    ) -> Result<Vec<(String, common::types::GatewayRouteConfig)>, PlatformError> {
        use redb::ReadableTable;
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx.open_table(tables::GATEWAY_CONFIGS).map_err(|e| {
            PlatformError::storage_with_msg("failed to open GATEWAY_CONFIGS table", e)
        })?;
        let mut configs = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| PlatformError::storage_with_msg("failed to iterate gateway configs", e))?
        {
            let (k, v) = entry.map_err(|e| {
                PlatformError::storage_with_msg("failed to read gateway config entry", e)
            })?;
            let app_id = k.value().to_string();
            let config: common::types::GatewayRouteConfig = serde_json::from_str(v.value())
                .map_err(|e| {
                    PlatformError::storage_with_msg("failed to deserialize gateway config", e)
                })?;
            configs.push((app_id, config));
        }
        Ok(configs)
    }

    // ── API Key Persistence ────────────────────────────────────────────────────

    /// Save API keys for an app.
    pub fn save_api_keys(
        &self,
        app_id: &str,
        keys: &[common::types::ApiKeyRecord],
    ) -> Result<(), PlatformError> {
        let json = serde_json::to_string(keys)
            .map_err(|e| PlatformError::storage_with_msg("failed to serialize api keys", e))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx
                .open_table(tables::API_KEYS)
                .map_err(|e| PlatformError::storage_with_msg("failed to open API_KEYS table", e))?;
            table
                .insert(app_id, json.as_str())
                .map_err(|e| PlatformError::storage_with_msg("failed to write api keys", e))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::storage_with_msg("failed to commit api keys", e))?;
        Ok(())
    }

    /// Load API keys for an app.
    pub fn load_api_keys(
        &self,
        app_id: &str,
    ) -> Result<Vec<common::types::ApiKeyRecord>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx
            .open_table(tables::API_KEYS)
            .map_err(|e| PlatformError::storage_with_msg("failed to open API_KEYS table", e))?;
        match table
            .get(app_id)
            .map_err(|e| PlatformError::storage_with_msg("failed to read api keys", e))?
        {
            Some(v) => {
                let keys: Vec<common::types::ApiKeyRecord> = serde_json::from_str(v.value())
                    .map_err(|e| {
                        PlatformError::storage_with_msg("failed to deserialize api keys", e)
                    })?;
                Ok(keys)
            }
            None => Ok(Vec::new()),
        }
    }

    /// Delete API keys for an app.
    pub fn delete_api_keys(&self, app_id: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx
                .open_table(tables::API_KEYS)
                .map_err(|e| PlatformError::storage_with_msg("failed to open API_KEYS table", e))?;
            table
                .remove(app_id)
                .map_err(|e| PlatformError::storage_with_msg("failed to delete api keys", e))?;
        }
        tx.commit().map_err(|e| {
            PlatformError::storage_with_msg("failed to commit api keys deletion", e)
        })?;
        Ok(())
    }
}
