#![allow(clippy::result_large_err)]

use redb::Database;
use std::path::Path;
use std::sync::Arc;

pub mod artifact;
pub mod artifact_server;
pub mod billing;
pub mod config;
mod core;
pub mod gc;
pub mod gc_metrics;
pub mod health;
pub mod integrity;
mod management_state;
pub mod metrics;
mod migrations;
pub mod routes;
pub mod secrets;
pub mod tables;

#[cfg(test)]
mod tests;

/// Current schema version.
const CURRENT_SCHEMA_VERSION: u32 = 8;

#[derive(Clone)]
pub struct Store {
    pub(crate) db: Arc<Database>,
    db_path: std::path::PathBuf,
}

impl Store {
    /// Open or create the store at the given path.
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        core::open_store(path)
    }

    /// Get a reference to the underlying database.
    pub fn db(&self) -> &Arc<Database> {
        &self.db
    }
}
