use redb::Database;
use std::path::Path;
use std::sync::Arc;

pub mod artifact;
pub mod config;
pub mod metrics;
pub mod secrets;
pub mod tables;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct Store {
    pub db: Arc<Database>,
}

impl Store {
    /// Open (or create) the database at the given path.
    /// Creates all table definitions on first run.
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;

        // Ensure tables exist (idempotent)
        let tx = db.begin_write()?;
        {
            tx.open_table(tables::ARTIFACTS)?;
            tx.open_table(tables::CONFIGS)?;
            tx.open_table(tables::SECRETS)?;
            tx.open_table(tables::METRICS)?;
            tx.open_table(tables::ROUTES)?;
            tx.open_table(tables::RAW_WASM)?;
            tx.open_table(tables::SCHEMA_META)?;
        }
        tx.commit()?;

        Ok(Store { db: Arc::new(db) })
    }
}
