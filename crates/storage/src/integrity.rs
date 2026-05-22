use crate::tables::{
    ARTIFACTS, ARTIFACT_HASHES, CONFIGS, METRICS, RAW_WASM, ROUTES, SCHEMA_META, SECRETS,
};
use crate::Store;
use common::{error::PlatformError, protocol::MessageEnvelope};
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
const ROUTE_REPLAY_BATCH_SIZE: usize = 256;
const ROUTE_REPLAY_EXPIRES_MS: u64 = 250;

#[derive(Debug)]
enum ReplayRouteEvent {
    Add(common::types::Route),
    Remove(String),
}

fn decode_replay_route_event(payload: &[u8]) -> Result<ReplayRouteEvent, PlatformError> {
    let event_value = if let Ok(envelope) =
        serde_json::from_slice::<MessageEnvelope<serde_json::Value>>(payload)
    {
        if !envelope.is_compatible() {
            return Err(PlatformError::messaging(format!(
                "incompatible protocol version {} during routes replay",
                envelope.protocol_version
            )));
        }
        envelope.payload
    } else {
        serde_json::from_slice::<serde_json::Value>(payload)
            .map_err(PlatformError::messaging_source)?
    };

    let event_type = event_value
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| PlatformError::messaging("route replay payload missing event type"))?;

    match event_type {
        "route_add" => {
            let route_obj = event_value.get("route").cloned().ok_or_else(|| {
                PlatformError::messaging("route_add replay payload missing route")
            })?;
            let route = serde_json::from_value::<common::types::Route>(route_obj)
                .map_err(PlatformError::messaging_source)?;
            Ok(ReplayRouteEvent::Add(route))
        }
        "route_remove" => {
            let host = event_value
                .get("host")
                .and_then(|h| h.as_str())
                .ok_or_else(|| {
                    PlatformError::messaging("route_remove replay payload missing host")
                })?
                .to_string();
            Ok(ReplayRouteEvent::Remove(host))
        }
        other => Err(PlatformError::messaging(format!(
            "unexpected event type during routes replay: {other}"
        ))),
    }
}

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
        use async_nats::jetstream::consumer::{
            pull::Config as PullConfig, AckPolicy, DeliverPolicy,
        };
        use futures::StreamExt;
        use std::time::Duration;

        let js = async_nats::jetstream::new(nats_client.clone());
        let stream = match js.get_stream("DEPLOY").await {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("DEPLOY stream not found, skipping routes replay");
                return Ok(());
            }
        };

        tracing::info!("creating ephemeral consumer for routes replay");
        let consumer = stream
            .create_consumer(PullConfig {
                // Ephemeral consumer so each rebuild replays full route history.
                durable_name: None,
                deliver_policy: DeliverPolicy::All,
                ack_policy: AckPolicy::Explicit,
                filter_subject: "routes.>".to_string(),
                inactive_threshold: Duration::from_secs(30),
                ..Default::default()
            })
            .await
            .map_err(|e| {
                PlatformError::messaging(format!("failed to create replay consumer: {e}"))
            })?;

        let mut processed = 0u64;
        loop {
            let mut messages = consumer
                .fetch()
                .max_messages(ROUTE_REPLAY_BATCH_SIZE)
                .expires(Duration::from_millis(ROUTE_REPLAY_EXPIRES_MS))
                .messages()
                .await
                .map_err(|e| {
                    PlatformError::messaging(format!("failed to fetch replay batch: {e}"))
                })?;

            let mut batch_count = 0usize;
            while let Some(msg) = messages.next().await {
                let msg = msg.map_err(|e| {
                    PlatformError::messaging(format!("error receiving route replay message: {e}"))
                })?;
                batch_count += 1;

                match decode_replay_route_event(&msg.payload)? {
                    ReplayRouteEvent::Add(route) => {
                        store.save_route(&route)?;
                    }
                    ReplayRouteEvent::Remove(host) => {
                        store.delete_route(&host)?;
                    }
                }

                msg.ack().await.map_err(|e| {
                    PlatformError::messaging(format!("failed to ack replayed route message: {e}"))
                })?;
                processed += 1;
            }

            if batch_count == 0 {
                break;
            }
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

#[cfg(test)]
mod tests {
    use super::{decode_replay_route_event, ReplayRouteEvent};
    use common::{
        protocol::MessageEnvelope,
        types::{AppId, Route},
    };
    use messaging::{events::Event, NatsBus};
    use std::{net::TcpListener, time::Duration};
    use tempfile::NamedTempFile;
    use testcontainers::{core::ContainerPort, runners::AsyncRunner, GenericImage, ImageExt};

    fn sample_route() -> Route {
        Route {
            host: "example.com".to_string(),
            app_id: AppId("app:v1".to_string()),
            path_prefix: "/".to_string(),
            strip_prefix: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn setup_container_runtime() {
        if std::env::var("DOCKER_HOST").is_ok() {
            return;
        }

        let podman_socket = std::path::Path::new("/run/user/1000/podman/podman.sock");
        if podman_socket.exists() {
            std::env::set_var("DOCKER_HOST", "unix:///run/user/1000/podman/podman.sock");
        }

        if std::env::var("TESTCONTAINERS_RYUK_DISABLED").is_err() {
            std::env::set_var("TESTCONTAINERS_RYUK_DISABLED", "true");
        }
    }

    fn reserve_host_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral test port")
            .local_addr()
            .expect("read ephemeral test port")
            .port()
    }

    async fn start_test_nats() -> (testcontainers::ContainerAsync<GenericImage>, String) {
        setup_container_runtime();

        let host_port = reserve_host_port();
        let image = GenericImage::new("nats", "latest")
            .with_mapped_port(host_port, ContainerPort::Tcp(4222))
            .with_cmd(vec!["-js"]);
        let container = image.start().await.expect("Failed to start NATS container");
        tokio::time::sleep(Duration::from_secs(2)).await;
        (container, format!("nats://127.0.0.1:{host_port}"))
    }

    fn route(host: &str, app_id: &str, updated_at: u64) -> Route {
        Route {
            host: host.to_string(),
            app_id: AppId(app_id.to_string()),
            path_prefix: "/".to_string(),
            strip_prefix: false,
            created_at: updated_at,
            updated_at,
        }
    }

    #[test]
    fn test_decode_replay_route_event_from_envelope() {
        let payload = serde_json::to_vec(&MessageEnvelope::new(
            "node-0",
            serde_json::json!({
                "type": "route_add",
                "route": sample_route(),
            }),
        ))
        .unwrap();

        match decode_replay_route_event(&payload).unwrap() {
            ReplayRouteEvent::Add(route) => assert_eq!(route.host, "example.com"),
            ReplayRouteEvent::Remove(_) => panic!("expected route_add"),
        }
    }

    #[test]
    fn test_decode_replay_route_event_from_legacy_bare_event() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "type": "route_remove",
            "host": "example.com",
        }))
        .unwrap();

        match decode_replay_route_event(&payload).unwrap() {
            ReplayRouteEvent::Remove(host) => assert_eq!(host, "example.com"),
            ReplayRouteEvent::Add(_) => panic!("expected route_remove"),
        }
    }

    #[tokio::test]
    async fn test_replay_routes_from_jetstream_restores_final_route_state() {
        let (_container, url) = start_test_nats().await;
        let mut bus = NatsBus::connect(&url).await.unwrap();
        bus.set_node_id("replay-test".to_string());
        bus.setup_jetstream().await.unwrap();

        let route_a_v1 = route("a.example.com", "app-a:v1", 10);
        let route_b_v1 = route("b.example.com", "app-b:v1", 20);
        let route_a_v2 = Route {
            updated_at: 30,
            ..route_a_v1.clone()
        };

        bus.publish(&Event::RouteAdd {
            route: route_a_v1.clone(),
        })
        .await
        .unwrap();
        bus.publish(&Event::RouteAdd {
            route: route_b_v1.clone(),
        })
        .await
        .unwrap();
        bus.publish(&Event::RouteRemove {
            host: route_a_v1.host.clone(),
        })
        .await
        .unwrap();
        bus.publish(&Event::RouteAdd {
            route: route_a_v2.clone(),
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        let temp_file = NamedTempFile::new().unwrap();
        let store = crate::Store::open(temp_file.path()).unwrap();
        store
            .save_route(&route("stale.example.com", "stale:v1", 1))
            .unwrap();

        store.recreate_table_routes().unwrap();
        crate::Store::replay_routes_from_jetstream(bus.client(), store.clone())
            .await
            .unwrap();

        let mut routes = store.list_routes().unwrap();
        routes.sort_by(|a, b| a.host.cmp(&b.host));

        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].host, "a.example.com");
        assert_eq!(routes[0].app_id.0, "app-a:v1");
        assert_eq!(routes[0].updated_at, 30);
        assert_eq!(routes[1].host, "b.example.com");
        assert_eq!(routes[1].app_id.0, "app-b:v1");
        assert!(store.load_route("stale.example.com").unwrap().is_none());
    }

    #[tokio::test]
    async fn test_replay_routes_from_jetstream_is_repeatable() {
        let (_container, url) = start_test_nats().await;
        let mut bus = NatsBus::connect(&url).await.unwrap();
        bus.set_node_id("replay-repeat-test".to_string());
        bus.setup_jetstream().await.unwrap();

        let route_a = route("repeat-a.example.com", "repeat-a:v1", 100);
        let route_b = route("repeat-b.example.com", "repeat-b:v1", 200);

        bus.publish(&Event::RouteAdd {
            route: route_a.clone(),
        })
        .await
        .unwrap();
        bus.publish(&Event::RouteAdd {
            route: route_b.clone(),
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        let temp_file = NamedTempFile::new().unwrap();
        let store = crate::Store::open(temp_file.path()).unwrap();

        store.recreate_table_routes().unwrap();
        crate::Store::replay_routes_from_jetstream(bus.client(), store.clone())
            .await
            .unwrap();
        let mut first = store.list_routes().unwrap();
        first.sort_by(|a, b| a.host.cmp(&b.host));

        store.recreate_table_routes().unwrap();
        crate::Store::replay_routes_from_jetstream(bus.client(), store.clone())
            .await
            .unwrap();
        let mut second = store.list_routes().unwrap();
        second.sort_by(|a, b| a.host.cmp(&b.host));

        assert_eq!(first, second);
    }
}
