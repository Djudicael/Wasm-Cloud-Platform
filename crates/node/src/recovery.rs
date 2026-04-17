use async_nats::Client as NatsClient;
use storage::Store;
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryMode {
    Normal,
    FullRebuild,
    CorruptionDetected,
}

pub fn detect_recovery_mode(store: &Store, node_id: &str) -> RecoveryMode {
    match store.count_artifacts() {
        Ok(0) => {
            info!(
                node = node_id,
                "empty redb detected — entering recovery mode"
            );
            RecoveryMode::FullRebuild
        }
        Ok(n) => {
            info!(
                node = node_id,
                artifacts = n,
                "existing state found — normal startup"
            );
            RecoveryMode::Normal
        }
        Err(e) => {
            warn!(node = node_id, error = %e, "redb read failed — corruption likely");
            RecoveryMode::CorruptionDetected
        }
    }
}

pub async fn startup_integrity_check(store: &Store, nats_client: &NatsClient) {
    let report = store.integrity_check();

    match &report.recommendation {
        storage::integrity::RecoveryAction::Healthy => {
            tracing::info!(tables = report.tables_ok, "startup integrity check passed");
        }
        storage::integrity::RecoveryAction::PartialRebuild { tables } => {
            tracing::warn!(
                corrupted = ?tables,
                "startup integrity check found corrupt tables — rebuilding"
            );
            if let Err(e) = store.partial_rebuild(tables, nats_client).await {
                tracing::error!(error = %e, "partial rebuild failed — manual intervention may be required");
            } else {
                tracing::info!("partial rebuild completed successfully");
            }
        }
        storage::integrity::RecoveryAction::FullRebootstrap => {
            tracing::error!("critical tables corrupted — triggering full re-bootstrap");
            match std::fs::remove_file(store.db_path()) {
                Ok(_) => {
                    tracing::info!("corrupted redb deleted — restart required for clean bootstrap")
                }
                Err(e) => {
                    tracing::error!(error = %e, path = ?store.db_path(), "failed to delete corrupted redb — manual cleanup may be required");
                }
            }
            std::process::exit(1);
        }
    }
}
