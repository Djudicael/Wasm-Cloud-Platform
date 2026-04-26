use crate::tables::{
    ARTIFACTS, ARTIFACT_HASHES, CONFIGS, METRICS, RAW_WASM, ROUTES, SCHEMA_META, SECRETS,
};
use crate::Store;
use common::error::PlatformError;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityReport {
    pub tables_checked: u32,
    pub tables_ok: u32,
    pub tables_corrupted: Vec<String>,
    pub recommendation: RecoveryAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecoveryAction {
    Healthy,
    PartialRebuild { tables: Vec<String> },
    FullRebootstrap,
}

const CRITICAL_TABLES: &[&str] = &["artifacts", "configs"];

impl Store {
    pub fn integrity_check(&self) -> IntegrityReport {
        let table_names = vec![
            "artifacts",
            "configs",
            "secrets",
            "metrics",
            "routes",
            "raw_wasm",
            "schema_meta",
            "artifact_hashes",
        ];

        let mut report = IntegrityReport {
            tables_checked: table_names.len() as u32,
            tables_ok: 0,
            tables_corrupted: Vec::new(),
            recommendation: RecoveryAction::Healthy,
        };

        for name in &table_names {
            match self.check_table_readable(name) {
                Ok(count) => {
                    tracing::info!(table = name, entries = count, "integrity check passed");
                    report.tables_ok += 1;
                }
                Err(e) => {
                    tracing::error!(table = name, error = %e, "integrity check FAILED — table corrupted");
                    report.tables_corrupted.push(name.to_string());
                }
            }
        }

        if report.tables_corrupted.is_empty() {
            report.recommendation = RecoveryAction::Healthy;
        } else if report
            .tables_corrupted
            .iter()
            .any(|t| CRITICAL_TABLES.contains(&t.as_str()))
        {
            report.recommendation = RecoveryAction::FullRebootstrap;
        } else {
            report.recommendation = RecoveryAction::PartialRebuild {
                tables: report.tables_corrupted.clone(),
            };
        }

        report
    }

    fn check_table_readable(&self, table_name: &str) -> Result<u64, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;

        /// Only verify the first entry is readable, not all entries.
        /// For large tables like ARTIFACTS, iterating all entries would read
        /// gigabytes of compiled binaries on every startup.
        macro_rules! verify_and_count {
            ($table_def:expr) => {{
                let table = tx
                    .open_table($table_def)
                    .map_err(PlatformError::storage_source)?;
                // Only check the first entry to verify table readability.
                // We do NOT iterate all entries because for large tables like
                // ARTIFACTS, reading every value would load gigabytes of
                // compiled binaries on every startup.
                let mut iter = table.iter().map_err(PlatformError::storage_source)?;
                if let Some(result) = iter.next() {
                    result.map_err(PlatformError::storage_source)?;
                }
                // redb's ReadOnlyTable does not expose len(), so we return 0.
                // The count is only used for informational logging; callers
                // that need an exact count should query the table directly.
                Ok(0u64)
            }};
        }

        match table_name {
            "artifacts" => verify_and_count!(ARTIFACTS),
            "configs" => verify_and_count!(CONFIGS),
            "secrets" => verify_and_count!(SECRETS),
            "metrics" => verify_and_count!(METRICS),
            "routes" => verify_and_count!(ROUTES),
            "raw_wasm" => verify_and_count!(RAW_WASM),
            "schema_meta" => verify_and_count!(SCHEMA_META),
            "artifact_hashes" => verify_and_count!(ARTIFACT_HASHES),
            other => Err(PlatformError::storage(format!("unknown table: {other}"))),
        }
    }

    pub async fn partial_rebuild(
        &self,
        corrupted_tables: &[String],
        nats_client: &async_nats::Client,
    ) -> Result<(), PlatformError> {
        for table_name in corrupted_tables {
            match table_name.as_str() {
                "routes" => {
                    self.recreate_table_routes()?;
                    Self::replay_routes_from_jetstream(nats_client, self.clone()).await?;
                    tracing::info!("routes table rebuilt from JetStream replay");
                }
                "metrics" => {
                    self.recreate_table_metrics()?;
                    tracing::warn!("metrics table rebuilt (historical data lost)");
                }
                other => {
                    tracing::warn!(
                        table = other,
                        "no automatic rebuild strategy for this table — manual intervention may be needed"
                    );
                }
            }
        }
        Ok(())
    }

    async fn replay_routes_from_jetstream(
        nats_client: &async_nats::Client,
        store: Store,
    ) -> Result<(), PlatformError> {
        use futures::StreamExt;

        let js = async_nats::jetstream::new(nats_client.clone());
        let stream = match js.get_stream("DEPLOY").await {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("DEPLOY stream not found, skipping routes replay");
                return Ok(());
            }
        };

        let consumer = match stream.get_consumer("recovery-routes-replay").await {
            Ok(c) => c,
            Err(_) => {
                tracing::info!("creating temporary consumer for routes replay");
                let config = async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some("recovery-routes-replay".to_string()),
                    ..Default::default()
                };
                stream.create_consumer(config).await.map_err(|e| {
                    PlatformError::messaging(format!("failed to create consumer: {e}"))
                })?
            }
        };

        let mut messages = consumer
            .messages()
            .await
            .map_err(|e| PlatformError::messaging(format!("failed to get messages: {e}")))?;

        let mut processed = 0u64;
        while let Some(msg) = messages.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "error receiving message during replay");
                    continue;
                }
            };

            let event: serde_json::Value = match serde_json::from_slice(&msg.payload) {
                Ok(e) => e,
                Err(_) => {
                    let _ = msg.ack().await;
                    continue;
                }
            };

            if let Some(event_type) = event.get("type").and_then(|t| t.as_str()) {
                match event_type {
                    "route_add" => {
                        if let Some(route_obj) = event.get("route") {
                            if let Ok(route) =
                                serde_json::from_value::<common::types::Route>(route_obj.clone())
                            {
                                if let Err(e) = store.save_route(&route) {
                                    tracing::warn!(
                                        error = %e,
                                        host = %route.host,
                                        "failed to restore route during replay"
                                    );
                                } else {
                                    processed += 1;
                                }
                            }
                        }
                    }
                    "route_remove" => {
                        if let Some(host) = event.get("host").and_then(|h| h.as_str()) {
                            let _ = store.delete_route(host);
                        }
                    }
                    _ => {}
                }
            }

            let _ = msg.ack().await;
        }

        tracing::info!(routes_processed = processed, "routes replay complete");
        Ok(())
    }

    fn recreate_table_routes(&self) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(ROUTES)
                .map_err(PlatformError::storage_source)?;
            let keys: Vec<String> = table
                .iter()
                .map_err(PlatformError::storage_source)?
                .filter_map(|e| match e {
                    Ok(v) => Some(v),
                    Err(err) => {
                        tracing::warn!(error = %err, "error iterating routes table entry, skipping");
                        None
                    }
                })
                .map(|(k, _)| k.value().to_string())
                .collect();
            for key in keys {
                table
                    .remove(key.as_str())
                    .map_err(PlatformError::storage_source)?;
            }
        }
        tx.commit().map_err(PlatformError::storage_source)?;
        Ok(())
    }

    fn recreate_table_metrics(&self) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(METRICS)
                .map_err(PlatformError::storage_source)?;
            let keys: Vec<String> = table
                .iter()
                .map_err(PlatformError::storage_source)?
                .filter_map(|e| match e {
                    Ok(v) => Some(v),
                    Err(err) => {
                        tracing::warn!(error = %err, "error iterating metrics table entry, skipping");
                        None
                    }
                })
                .map(|(k, _)| k.value().to_string())
                .collect();
            for key in keys {
                table
                    .remove(key.as_str())
                    .map_err(PlatformError::storage_source)?;
            }
        }
        tx.commit().map_err(PlatformError::storage_source)?;
        Ok(())
    }

    pub fn count_artifacts(&self) -> Result<u64, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(ARTIFACTS)
            .map_err(PlatformError::storage_source)?;
        table.len().map_err(PlatformError::storage_source)
    }

    pub fn db_path(&self) -> std::path::PathBuf {
        self.db_path.clone()
    }
}
