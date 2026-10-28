//! Durable broker ordering fences. Retained independently of artifact GC.
use crate::{tables::SCHEMA_META, Store};
use common::{error::PlatformError, types::AppId};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Cursor {
    sequence: u64,
    complete: bool,
}

fn key(stream: &str, epoch: &str, app: &AppId) -> String {
    format!(
        "lifecycle_cursor:{}",
        serde_json::to_string(&(stream, epoch, &app.0)).expect("strings serialize")
    )
}

impl Store {
    /// Persist before side effects. Pending deliveries may retry; older or
    /// completed deliveries cannot undo a newer control-plane intent.
    pub fn begin_lifecycle_event(
        &self,
        stream: &str,
        epoch: &str,
        sequence: u64,
        app: &AppId,
        remove: bool,
    ) -> Result<bool, PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(SCHEMA_META)
                .map_err(PlatformError::storage_source)?;
            let key = key(stream, epoch, app);
            let previous: Option<Cursor> = table
                .get(key.as_str())
                .map_err(PlatformError::storage_source)?
                .map(|v| serde_json::from_str(v.value()))
                .transpose()
                .map_err(PlatformError::storage_source)?;
            if previous.is_some_and(|old| {
                old.sequence > sequence || (old.sequence == sequence && old.complete)
            }) {
                return Ok(false);
            }
            let value = serde_json::to_string(&Cursor {
                sequence,
                complete: false,
            })
            .map_err(PlatformError::storage_source)?;
            table
                .insert(key.as_str(), value.as_str())
                .map_err(PlatformError::storage_source)?;
            if remove {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .to_string();
                table
                    .insert(format!("_undeploy:{}", app.0).as_str(), timestamp.as_str())
                    .map_err(PlatformError::storage_source)?;
            }
        }
        tx.commit().map_err(PlatformError::storage_source)?;
        Ok(true)
    }

    pub fn complete_lifecycle_event(
        &self,
        stream: &str,
        epoch: &str,
        sequence: u64,
        app: &AppId,
    ) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(SCHEMA_META)
                .map_err(PlatformError::storage_source)?;
            let key = key(stream, epoch, app);
            let previous: Option<Cursor> = table
                .get(key.as_str())
                .map_err(PlatformError::storage_source)?
                .map(|v| serde_json::from_str(v.value()))
                .transpose()
                .map_err(PlatformError::storage_source)?;
            if previous.is_some_and(|old| old.sequence == sequence) {
                let value = serde_json::to_string(&Cursor {
                    sequence,
                    complete: true,
                })
                .map_err(PlatformError::storage_source)?;
                table
                    .insert(key.as_str(), value.as_str())
                    .map_err(PlatformError::storage_source)?;
            }
        }
        tx.commit().map_err(PlatformError::storage_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_fence_survives_restart_and_allows_pending_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let app = AppId("agents/scheduler:v4".into());
        let store = Store::open(&path).unwrap();
        assert!(store
            .begin_lifecycle_event("DEPLOY", "epoch1", 10, &app, false)
            .unwrap());
        assert!(store
            .begin_lifecycle_event("DEPLOY", "epoch1", 11, &app, true)
            .unwrap());
        assert!(store.is_undeployed(&app.0).unwrap());
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(!store
            .begin_lifecycle_event("DEPLOY", "epoch1", 10, &app, false)
            .unwrap());
        assert!(store
            .begin_lifecycle_event("DEPLOY", "epoch1", 11, &app, true)
            .unwrap());
        store
            .complete_lifecycle_event("DEPLOY", "epoch1", 11, &app)
            .unwrap();
        assert!(!store
            .begin_lifecycle_event("DEPLOY", "epoch1", 11, &app, true)
            .unwrap());
        assert!(store
            .begin_lifecycle_event("DEPLOY", "epoch1", 12, &app, false)
            .unwrap());
        // A stale completion cannot complete a newer pending deployment.
        store
            .complete_lifecycle_event("DEPLOY", "epoch1", 11, &app)
            .unwrap();
        assert!(store
            .begin_lifecycle_event("DEPLOY", "epoch1", 12, &app, false)
            .unwrap());
        assert!(store
            .begin_lifecycle_event("DEPLOY", "epoch2", 1, &app, false)
            .unwrap());
        assert!(store
            .begin_lifecycle_event("DEPLOY", "epoch1", 1, &AppId("other:v1".into()), false)
            .unwrap());
    }
}
