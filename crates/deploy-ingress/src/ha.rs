use crate::{
    app::{AppState, LeaderLeaseRecord, DEPLOY_INGRESS_LEASE_KEY},
    audit::now_unix_secs,
};
use anyhow::Context;
use async_nats::jetstream;
use common::{
    error::PlatformError, health::NodeHealthStatus, protocol::PROTOCOL_VERSION,
    types::ClusterNodeRecord,
};
use messaging::{events::Event, NatsBus};
use sha2::Digest;
use storage::Store;
use tracing::warn;

pub async fn publish_artifact_replication(
    state: &AppState,
    ingress: &common::deploy::RemoteArtifactIngressResponse,
) -> Result<(), PlatformError> {
    if !state.ha_enabled {
        return Ok(());
    }
    state
        .bus
        .publish(&Event::DeployIngressArtifactReplicated {
            source_ingress_id: state.ingress_id.clone(),
            artifact_url: ingress.artifact_url.clone(),
            expected_hash: ingress.expected_hash.clone(),
            size_bytes: ingress.size_bytes,
        })
        .await
}

pub async fn subscribe_artifact_replication(state: &AppState) -> Result<(), PlatformError> {
    let state = state.clone();
    let consumer_name = format!("deploy_ingress_replication_{}", state.ingress_id);
    let bus = state.bus.clone();
    bus.subscribe_durable(
        "PLATFORM",
        &consumer_name,
        Some("platform.deploy_ingress.artifact.>"),
        move |event| {
            let state = state.clone();
            async move {
                if let Event::DeployIngressArtifactReplicated {
                    source_ingress_id,
                    artifact_url,
                    expected_hash,
                    ..
                } = event
                {
                    if source_ingress_id != state.ingress_id {
                        replicate_artifact_locally(&state, &artifact_url, &expected_hash).await?;
                    }
                }
                Ok(())
            }
        },
    )
    .await
}

async fn replicate_artifact_locally(
    state: &AppState,
    artifact_url: &str,
    expected_hash: &str,
) -> Result<(), PlatformError> {
    if state.store.raw_wasm_exists(expected_hash)? {
        return Ok(());
    }
    let response = reqwest::Client::new()
        .get(artifact_url)
        .send()
        .await
        .map_err(PlatformError::external_source)?;
    if !response.status().is_success() {
        return Err(PlatformError::external(format!(
            "artifact replication fetch failed with HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(PlatformError::external_source)?;
    let actual_hash = hex::encode(sha2::Sha256::digest(&bytes));
    if actual_hash != expected_hash {
        return Err(PlatformError::security(format!(
            "artifact replication hash mismatch: expected {}, got {}",
            expected_hash, actual_hash
        )));
    }
    state.store.save_raw_wasm(expected_hash, &bytes)?;
    Ok(())
}

pub async fn subscribe_cluster_updates(bus: &NatsBus, store: Store) -> Result<(), PlatformError> {
    let store_for_load = store.clone();
    bus.subscribe("node.load.>", move |event| {
        let store = store_for_load.clone();
        async move {
            if let Event::NodeLoad {
                node_id,
                active_instances,
                proxy_address,
                ..
            } = event
            {
                let now = now_unix_secs();
                let mut record = store
                    .load_cluster_node(&node_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| ClusterNodeRecord {
                        node_id: node_id.clone(),
                        last_seen_unix_secs: now,
                        joined_at_unix_secs: Some(now),
                        health_status: NodeHealthStatus::Healthy,
                        proxy_address: Some(proxy_address.clone()),
                        artifact_server_url: None,
                        protocol_version: Some(PROTOCOL_VERSION),
                        binary_version: None,
                        secret_transport_public_key: None,
                        accepting_requests: Some(true),
                        active_instances: Some(active_instances),
                        deployed_apps: None,
                    });
                record.last_seen_unix_secs = now;
                record.proxy_address = Some(proxy_address);
                record.active_instances = Some(active_instances);
                let _ = store.save_cluster_node(&record);
            }
        }
    })
    .await?;

    let store_for_join = store.clone();
    bus.subscribe("cluster.node_joined.>", move |event| {
        let store = store_for_join.clone();
        async move {
            if let Event::NodeJoined {
                node_id,
                artifact_server_url,
                protocol_version,
                binary_version,
                ..
            } = event
            {
                let now = now_unix_secs();
                let mut record = store
                    .load_cluster_node(&node_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| ClusterNodeRecord {
                        node_id: node_id.clone(),
                        last_seen_unix_secs: now,
                        joined_at_unix_secs: Some(now),
                        health_status: NodeHealthStatus::Healthy,
                        proxy_address: None,
                        artifact_server_url: Some(artifact_server_url.clone()),
                        protocol_version: Some(protocol_version),
                        binary_version: Some(binary_version.clone()),
                        secret_transport_public_key: None,
                        accepting_requests: Some(true),
                        active_instances: Some(0),
                        deployed_apps: Some(0),
                    });
                record.last_seen_unix_secs = now;
                record.artifact_server_url = Some(artifact_server_url);
                record.protocol_version = Some(protocol_version);
                if !binary_version.trim().is_empty() {
                    record.binary_version = Some(binary_version);
                }
                let _ = store.save_cluster_node(&record);
            }
        }
    })
    .await?;

    Ok(())
}

pub async fn get_or_create_kv(
    js: &jetstream::Context,
    bucket: &str,
    max_age: Option<std::time::Duration>,
) -> anyhow::Result<jetstream::kv::Store> {
    match js
        .create_key_value(jetstream::kv::Config {
            bucket: bucket.to_string(),
            history: 1,
            max_age: max_age.unwrap_or_default(),
            ..Default::default()
        })
        .await
    {
        Ok(store) => Ok(store),
        Err(_) => js
            .get_key_value(bucket)
            .await
            .with_context(|| format!("failed to create or load KV bucket {bucket}")),
    }
}

// Leader election stays in JetStream KV so every ingress instance can observe the same
// source of truth without introducing another coordinator service.
pub fn start_leader_lease_task(
    state: AppState,
    leader_kv: jetstream::kv::Store,
    refresh_interval: std::time::Duration,
) {
    tokio::spawn(async move {
        let mut last_revision: Option<u64> = None;
        loop {
            if let Err(err) = renew_or_observe_leader(&state, &leader_kv, &mut last_revision).await
            {
                warn!(error = %err, "deploy ingress leader lease update failed");
            }
            tokio::time::sleep(refresh_interval).await;
        }
    });
}

async fn renew_or_observe_leader(
    state: &AppState,
    leader_kv: &jetstream::kv::Store,
    last_revision: &mut Option<u64>,
) -> Result<(), PlatformError> {
    let record = LeaderLeaseRecord {
        ingress_id: state.ingress_id.clone(),
        artifact_server_url: state.artifact_server_url.clone(),
        updated_at_unix_secs: now_unix_secs(),
    };
    let payload = serde_json::to_vec(&record)?;

    if let Some(revision) = *last_revision {
        match leader_kv
            .update(DEPLOY_INGRESS_LEASE_KEY, payload.clone().into(), revision)
            .await
        {
            Ok(new_revision) => {
                *last_revision = Some(new_revision);
                set_leader_state(state, &record, true).await;
                return Ok(());
            }
            Err(err) => {
                warn!(error = %err, "deploy ingress leader lease renew lost; observing current leader");
                *last_revision = None;
            }
        }
    }

    match leader_kv
        .create(DEPLOY_INGRESS_LEASE_KEY, payload.into())
        .await
    {
        Ok(revision) => {
            *last_revision = Some(revision);
            set_leader_state(state, &record, true).await;
            Ok(())
        }
        Err(_) => {
            let current = leader_kv
                .entry(DEPLOY_INGRESS_LEASE_KEY)
                .await
                .map_err(PlatformError::messaging_source)?;
            if let Some(entry) = current {
                let leader: LeaderLeaseRecord = serde_json::from_slice(entry.value.as_ref())?;
                if leader.ingress_id == state.ingress_id {
                    *last_revision = Some(entry.revision);
                    set_leader_state(state, &leader, true).await;
                } else {
                    *last_revision = None;
                    set_leader_state(state, &leader, false).await;
                }
            } else {
                set_unknown_leader_state(state).await;
            }
            Ok(())
        }
    }
}

async fn set_leader_state(state: &AppState, leader: &LeaderLeaseRecord, is_leader: bool) {
    let mut guard = state.leader_state.write().await;
    guard.is_leader = is_leader;
    guard.leader_ingress_id = Some(leader.ingress_id.clone());
    guard.leader_artifact_server_url = Some(leader.artifact_server_url.clone());
}

async fn set_unknown_leader_state(state: &AppState) {
    let mut guard = state.leader_state.write().await;
    guard.is_leader = false;
    guard.leader_ingress_id = None;
    guard.leader_artifact_server_url = None;
}
