# Step 22 — Storage Schema Versioning & Migration

## Goal
`redb` is an embedded database with no built-in schema migration system.

---

## Context & Rationale

### The Problem This Solves

The platform stores `AppConfig` as JSON in redb. In step 21, we added a new field
`db_max_connections` to `AppConfig`. What happens when a node binary with this new
field starts on a machine that has an older redb file?

Without a migration system: `serde_json::from_str::<AppConfig>()` will fail because
the stored JSON is missing `db_max_connections`. The node crashes on startup.

With a migration system: the node detects that the stored schema is v1, applies the
`v1 → v2` migration (adds `db_max_connections = 10` to all existing records), then
starts normally.

Schema migration is a solved problem in every database system. This step adapts the
standard approach for redb's embedded environment.

### Why an Explicit Version Number (Not Schema Hash)?

Some systems detect schema changes by hashing the table definitions. This is fragile:
- A comment change in the Rust code would change the hash
- Reordering fields would change the hash even if the data format is compatible

An explicit integer version number is clearer:
- v1 = initial schema
- v2 = added `db_max_connections` to AppConfig
- v3 = added `wasm_component_model` flag

The version number is stored in the `_schema_meta` table and bumped explicitly by the
developer when they write a migration. No surprises.

### Why Downgrade Is Not Supported

If the database is version 3 and you run the binary for version 2, the binary would
try to read records in v3 format with v2 structs → fields missing → deserialization
failures → crashes.

The correct response to a database that is newer than the binary is a clear error:
```
Database schema version 3 is NEWER than the binary supports (2).
Downgrade is not supported. Use a newer binary.
```

Downgrade support would require every migration to be reversible (up + down). This
doubles the migration code and is almost never needed in practice. If downgrade is
required, the operator restores from the pre-migration backup (which we always create).

### Why Back Up Before Migration?

A migration that fails halfway through can leave the database in a partially-migrated
state. For example:
- Migration v1→v2 processes 1000 records
- It fails at record 500 (disk full)
- Records 1–500 are in v2 format; records 501–1000 are in v1 format
- The schema version was NOT bumped yet (we use a single transaction per migration)
- On restart, the migration runs again from the beginning... but records 1–500 are
  already in v2 format and the migration must handle this gracefully

Migrations are written to be idempotent: applying them to already-migrated data is a
no-op. The backup is a safety net for catastrophic failures (power loss, disk corruption).

### Migration Transaction Safety

Each migration runs inside a **single write transaction**. Either the entire migration
succeeds and the version number is bumped (in the same transaction), or the entire
migration rolls back and the version number is unchanged. This prevents partially-applied
migrations from being misdetected as complete.

```
begin_write()
  apply_migration(v → v+1)   ← all table writes happen here
  write_schema_version(v+1)  ← version bump is inside the same transaction
commit()                      ← atomic: either all succeed or all rollback
```

--- As the platform
evolves, table structures will change (new fields in `AppConfig`, new tables, renamed tables).
Without a migration system, updating the node binary will corrupt or fail to read existing data.

---

## 1. Schema Version Table

Add a special table that holds the current schema version number.

```rust
// crates/storage/src/tables.rs (add)
/// Stores a single key "version" with the schema version as a u32.
pub const SCHEMA_META: TableDefinition<&str, u32> = TableDefinition::new("_schema_meta");
```

```rust
// crates/storage/src/lib.rs
const CURRENT_SCHEMA_VERSION: u32 = 1;

impl Store {
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;

        // Initialize tables
        let tx = db.begin_write()?;
        {
            tx.open_table(tables::SCHEMA_META)?;
            tx.open_table(tables::ARTIFACTS)?;
            tx.open_table(tables::RAW_WASM)?;
            tx.open_table(tables::CONFIGS)?;
            tx.open_table(tables::SECRETS)?;
            tx.open_table(tables::METRICS)?;
            tx.open_table(tables::ROUTES)?;
        }
        tx.commit()?;

        let store = Store { db: Arc::new(db) };
        store.run_migrations()?;
        Ok(store)
    }

    fn read_schema_version(&self) -> Result<u32, redb::Error> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(tables::SCHEMA_META)?;
        Ok(table.get("version")?.map(|v| v.value()).unwrap_or(0))
    }

    fn write_schema_version(&self, version: u32) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META)?;
            table.insert("version", version)?;
        }
        tx.commit()
    }

    fn run_migrations(&self) -> Result<(), redb::Error> {
        let current = self.read_schema_version()?;
        tracing::info!(current, target = CURRENT_SCHEMA_VERSION, "checking schema version");

        if current == CURRENT_SCHEMA_VERSION {
            return Ok(());
        }

        if current > CURRENT_SCHEMA_VERSION {
            panic!(
                "Database schema version {current} is NEWER than the binary supports ({CURRENT_SCHEMA_VERSION}). \
                 Downgrade is not supported. Use a newer binary."
            );
        }

        // Apply migrations in order
        for v in current..CURRENT_SCHEMA_VERSION {
            tracing::info!(from = v, to = v + 1, "running migration");
            self.apply_migration(v + 1)?;
            self.write_schema_version(v + 1)?;
            tracing::info!(version = v + 1, "migration complete");
        }

        Ok(())
    }

    fn apply_migration(&self, target_version: u32) -> Result<(), redb::Error> {
        match target_version {
            1 => {
                // v0 → v1: first schema, nothing to migrate (fresh install)
                // Just ensure all tables exist (already done in open())
                tracing::info!("schema v1: initial schema created");
            }
            2 => {
                // v1 → v2: Example future migration
                // Add "db_max_connections" field to all AppConfig records.
                // Old records stored as JSON — we rewrite them with the new default.
                migrate_v1_to_v2(self)?;
            }
            n => panic!("Unknown migration target: {n}"),
        }
        Ok(())
    }
}
```

---

## 2. Migration Example: v1 → v2

```rust
// crates/storage/src/lib.rs
fn migrate_v1_to_v2(store: &Store) -> Result<(), redb::Error> {
    use crate::tables::CONFIGS;

    let tx = store.db.begin_read()?;
    let table = tx.open_table(CONFIGS)?;

    // Read all existing records
    let records: Vec<(String, String)> = table.iter()?
        .filter_map(|e| e.ok())
        .map(|(k, v)| (k.value().to_string(), v.value().to_string()))
        .collect();
    drop(table);
    drop(tx);

    // Rewrite with new default field
    let write_tx = store.db.begin_write()?;
    {
        let mut table = write_tx.open_table(CONFIGS)?;
        for (key, json_str) in records {
            // Parse as generic JSON (schema-agnostic)
            if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&json_str) {
                // Add the new field with a safe default
                if val.get("db_max_connections").is_none() {
                    val["db_max_connections"] = serde_json::json!(10);
                }
                let new_json = serde_json::to_string(&val)
                    .expect("re-serialize failed");
                table.insert(key.as_str(), new_json.as_str())?;
            }
        }
    }
    write_tx.commit()?;
    tracing::info!("v1→v2: added db_max_connections to all app configs");
    Ok(())
}
```

---

## 3. Migration Rules

| Rule | Reason |
|------|--------|
| Migrations are idempotent | A crash mid-migration and restart must not corrupt data |
| Migrations run inside a single write transaction | If the migration fails, the version number is NOT written |
| JSON fields in configs use `Option<T>` or defaults | Old records missing new fields must be gracefully handled |
| Never rename or drop a table in a patch release | Only in major schema versions |
| Always back up redb before a schema bump | Add to deployment checklist |

---

## 4. Backup Before Migration

```rust
// crates/storage/src/lib.rs (add before run_migrations)
fn backup_if_needed(&self, path: &Path) -> Result<(), std::io::Error> {
    let current = self.read_schema_version().unwrap_or(0);
    if current < CURRENT_SCHEMA_VERSION {
        let backup_path = path.with_extension(
            format!("redb.v{current}.bak")
        );
        std::fs::copy(path, &backup_path)?;
        tracing::warn!(
            backup = %backup_path.display(),
            "Created backup before migration"
        );
    }
    Ok(())
}
```

Call `backup_if_needed(path)?` before `run_migrations()`.

---

## 5. Schema Version History

Maintain this table in the source code as a changelog.

```
Version │ Binary version │ Changes
────────┼────────────────┼───────────────────────────────────────────────────
  0     │  (fresh DB)    │  No schema written yet
  1     │  0.1.0         │  Initial: artifacts, raw_wasm, configs, secrets,
        │                │  metrics, routes, _schema_meta
  2     │  0.2.0 (TBD)   │  Add db_max_connections to AppConfig
        │                │  Add artifact_sha256 metadata table
  3     │  0.3.0 (TBD)   │  Add wasm_component_model bool to AppConfig
        │                │  Add log_level field to AppConfig
```

---

## 6. Artifact SHA-256 Metadata Table

Referenced in step 19 (cluster bootstrap) — tracks the sha256 of each compiled app
so the cluster can push artifacts to new nodes.

```rust
// crates/storage/src/tables.rs (add in v2)
/// Key   : app_id
/// Value : hex sha256 of the original .wasm
pub const ARTIFACT_META: TableDefinition<&str, &str> = TableDefinition::new("artifact_meta");
```

```rust
// crates/storage/src/artifact.rs (add)
impl Store {
    pub fn save_artifact_sha256(&self, id: &AppId, sha256: &str) -> Result<(), PlatformError> {
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(crate::tables::ARTIFACT_META)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(id.0.as_str(), sha256)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn get_artifact_sha256(&self, id: &AppId) -> Result<Option<String>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(crate::tables::ARTIFACT_META)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        Ok(table.get(id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .map(|v| v.value().to_string()))
    }
}
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Version Tracking
- [ ] A fresh database starts at version 0 (no entry in `_schema_meta`)
- [ ] After `Store::open()`, `read_schema_version()` returns `CURRENT_SCHEMA_VERSION`
- [ ] The version number is written atomically with the migration (version is not bumped if migration fails)

### Migration Runner
- [ ] `run_migrations()` runs all pending migrations in order (v0→v1→v2, not skipping)
- [ ] `run_migrations()` is a no-op when the database is already at `CURRENT_SCHEMA_VERSION`
- [ ] Opening a database newer than the binary's `CURRENT_SCHEMA_VERSION` panics with a clear message (downgrade not supported)
- [ ] A failed migration does not leave the database in a partially-migrated state (transaction rollback)

### Backup
- [ ] `backup_if_needed()` creates a `.redb.v<N>.bak` file before any migration runs
- [ ] The backup is created only when a migration is actually needed (not on every startup)
- [ ] If the backup file already exists, it is not overwritten (prevents losing the pre-migration state)

### Migration v1→v2 (Example)
- [ ] All existing `AppConfig` records get the `db_max_connections` field added with value `10`
- [ ] Records already having the field are not modified
- [ ] After migration, `load_config()` deserializes all records without error

### Version History Table
- [ ] The version history comment in the source code is updated whenever a new migration is added
- [ ] Every migration has a matching entry in the `Schema Version History` table in this file

### Tests
- [ ] A test opens a fresh database and verifies the schema version is `CURRENT_SCHEMA_VERSION`
- [ ] A test writes data with the old schema structure, then runs `apply_migration(2)`, and verifies the data is correctly transformed
- [ ] A test verifies that opening a database from a future version panics with the expected message
