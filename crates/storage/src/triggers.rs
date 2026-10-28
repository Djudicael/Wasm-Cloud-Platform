//! Durable high-water marks, not a work queue. Missed ticks are never replayed.
use crate::{tables::SCHEMA_META, Store};
use common::{error::PlatformError, triggers::ScheduleTrigger, types::AppId};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Cursor {
    declaration: ScheduleTrigger,
    bucket: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TriggerTick {
    pub scheduled_at_ms: u64,
    pub missed: u64,
}

pub fn cursor_key(node: &str, app: &AppId, id: &str) -> String {
    format!(
        "schedule_cursor:{}",
        serde_json::to_string(&(node, &app.0, id)).expect("string tuple serializes")
    )
}

impl Store {
    pub(crate) fn delete_trigger_cursors_for_app(
        &self,
        app_name: &str,
    ) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(SCHEMA_META)
                .map_err(PlatformError::storage_source)?;
            let mut keys = Vec::new();
            for entry in table.iter().map_err(PlatformError::storage_source)? {
                let (key, _) = entry.map_err(PlatformError::storage_source)?;
                if let Some(value) = key.value().strip_prefix("schedule_cursor:") {
                    let (_, app, _): (String, String, String) =
                        serde_json::from_str(value).map_err(PlatformError::storage_source)?;
                    if app == app_name || app.starts_with(&format!("{app_name}:")) {
                        keys.push(key.value().to_string());
                    }
                }
            }
            for key in keys {
                table
                    .remove(key.as_str())
                    .map_err(PlatformError::storage_source)?;
            }
        }
        tx.commit().map_err(PlatformError::storage_source)
    }
    /// Commit before dispatch: a crash can lose a tick but cannot replay a bucket.
    pub fn advance_trigger_cursor(
        &self,
        node: &str,
        app: &AppId,
        declaration: &ScheduleTrigger,
        now_ms: u64,
    ) -> Result<Option<TriggerTick>, PlatformError> {
        common::triggers::validate(&[common::triggers::Trigger::Schedule(declaration.clone())])
            .map_err(PlatformError::ConfigValidation)?;
        let interval = common::triggers::duration(&declaration.every)
            .map_err(PlatformError::ConfigValidation)?
            .as_millis() as u64;
        let bucket = now_ms / interval;
        let key = cursor_key(node, app, &declaration.id);
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        let tick;
        {
            let mut table = tx
                .open_table(SCHEMA_META)
                .map_err(PlatformError::storage_source)?;
            let previous: Option<Cursor> = table
                .get(key.as_str())
                .map_err(PlatformError::storage_source)?
                .map(|v| serde_json::from_str(v.value()))
                .transpose()
                .map_err(PlatformError::storage_source)?;
            tick = match previous {
                Some(old) if old.declaration == *declaration => {
                    if bucket <= old.bucket {
                        return Ok(None);
                    }
                    Some(TriggerTick {
                        scheduled_at_ms: bucket * interval,
                        missed: bucket - old.bucket - 1,
                    })
                }
                _ => None, // New/changed schedules start at the next boundary.
            };
            let value = serde_json::to_string(&Cursor {
                declaration: declaration.clone(),
                bucket,
            })
            .map_err(PlatformError::storage_source)?;
            table
                .insert(key.as_str(), value.as_str())
                .map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)?;
        Ok(tick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_skips_backlog_and_clock_rollback_does_not_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");
        let app = AppId::new("scheduled", "v1");
        let s = ScheduleTrigger {
            id: "tick".into(),
            every: "1s".into(),
            route: "/_platform/triggers/tick".into(),
            method: "POST".into(),
            max_concurrency: 1,
            timeout: "1s".into(),
        };
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(
                store.advance_trigger_cursor("n1", &app, &s, 1_500).unwrap(),
                None
            );
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.advance_trigger_cursor("n1", &app, &s, 5_100).unwrap(),
            Some(TriggerTick {
                scheduled_at_ms: 5_000,
                missed: 3
            })
        );
        for now in [5_999, 2_000] {
            assert_eq!(
                store.advance_trigger_cursor("n1", &app, &s, now).unwrap(),
                None
            );
        }
        assert_eq!(
            store.advance_trigger_cursor("n2", &app, &s, 5_100).unwrap(),
            None
        );
        assert_eq!(
            store
                .advance_trigger_cursor("n1", &app, &s, 6_100)
                .unwrap()
                .unwrap()
                .missed,
            0
        );
        store.delete_trigger_cursors_for_app(&app.0).unwrap();
        assert_eq!(
            store.advance_trigger_cursor("n1", &app, &s, 8_100).unwrap(),
            None
        );
    }
}
