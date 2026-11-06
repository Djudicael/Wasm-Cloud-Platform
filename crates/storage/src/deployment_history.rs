//! Append-only observed deployment content and events, atomically committed
//! with the corresponding configuration/artifact update. Config values stay
//! private; control APIs must project them explicitly.
use crate::{tables::SCHEMA_META, Store};
use common::{error::PlatformError, types::AppConfig};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
fn app_key(app: &str) -> String {
    hex::encode(Sha256::digest(app.as_bytes()))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    pub revision: String,
    pub config: AppConfig,
    pub artifact_sha256: String,
    pub recorded_at_ms: u64,
    pub sequence: u64,
}
pub(crate) fn record(
    tx: &redb::WriteTransaction,
    config: &AppConfig,
    artifact: &str,
) -> Result<(), PlatformError> {
    let value = serde_json::to_value((config, artifact)).map_err(PlatformError::storage_source)?;
    let revision = hex::encode(Sha256::digest(
        serde_json::to_vec(&value).map_err(PlatformError::storage_source)?,
    ));
    let mut table = tx
        .open_table(SCHEMA_META)
        .map_err(PlatformError::storage_source)?;
    let seq = table
        .get("deployment-history-sequence")
        .map_err(PlatformError::storage_source)?
        .map(|v| v.value().parse::<u64>())
        .transpose()
        .map_err(PlatformError::storage_source)?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| PlatformError::storage("deployment sequence exhausted"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(PlatformError::storage_source)?
        .as_millis() as u64;
    let record = Revision {
        revision: revision.clone(),
        config: config.clone(),
        artifact_sha256: artifact.into(),
        recorded_at_ms: now,
        sequence: seq,
    };
    let raw = serde_json::to_string(&record).map_err(PlatformError::storage_source)?;
    table
        .insert("deployment-history-sequence", seq.to_string().as_str())
        .map_err(PlatformError::storage_source)?;
    table
        .insert(
            format!("deployment-revision:{}|{revision}", app_key(&config.id.0)).as_str(),
            raw.as_str(),
        )
        .map_err(PlatformError::storage_source)?;
    table
        .insert(
            format!("deployment-event:{}|{seq:020}", app_key(&config.id.0)).as_str(),
            raw.as_str(),
        )
        .map_err(PlatformError::storage_source)?;
    Ok(())
}
impl Store {
    pub fn deployment_revision(
        &self,
        app: &str,
        revision: &str,
    ) -> Result<Option<Revision>, PlatformError> {
        self.load_meta(&format!("deployment-revision:{}|{revision}", app_key(app)))
            .map_err(PlatformError::storage_source)?
            .map(|raw| serde_json::from_str(&raw).map_err(PlatformError::storage_source))
            .transpose()
    }
    pub fn deployment_history(
        &self,
        app: &str,
        limit: usize,
    ) -> Result<Vec<Revision>, PlatformError> {
        let prefix = format!("deployment-event:{}|", app_key(app));
        self.meta_prefix(&prefix, limit)?
            .into_iter()
            .map(|(_, raw)| serde_json::from_str(&raw).map_err(PlatformError::storage_source))
            .collect()
    }
    pub fn deployment_history_before(
        &self,
        app: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Revision>, PlatformError> {
        if limit > 128 {
            return Err(PlatformError::ConfigValidation(
                "history query exceeds limit".into(),
            ));
        }
        let prefix = format!("deployment-event:{}|", app_key(app));
        let end = before
            .map(|n| format!("{prefix}{n:020}"))
            .unwrap_or_else(|| format!("{prefix}\u{10ffff}"));
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(SCHEMA_META)
            .map_err(PlatformError::storage_source)?;
        table
            .range(prefix.as_str()..end.as_str())
            .map_err(PlatformError::storage_source)?
            .rev()
            .take(limit)
            .map(|e| {
                let (_, raw) = e.map_err(PlatformError::storage_source)?;
                serde_json::from_str(raw.value()).map_err(PlatformError::storage_source)
            })
            .collect()
    }
    pub fn meta_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PlatformError> {
        if limit > 1024 {
            return Err(PlatformError::ConfigValidation(
                "metadata query exceeds limit".into(),
            ));
        }
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(SCHEMA_META)
            .map_err(PlatformError::storage_source)?;
        let end = format!("{prefix}\u{10ffff}");
        table
            .range(prefix..end.as_str())
            .map_err(PlatformError::storage_source)?
            .rev()
            .take(limit)
            .map(|e| {
                e.map(|(k, v)| (k.value().to_owned(), v.value().to_owned()))
                    .map_err(PlatformError::storage_source)
            })
            .collect()
    }
}
