use async_nats::Client as NatsClient;
use common::config::{StorageIntegrityFailureMode, StorageSection};
use std::path::{Path, PathBuf};
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

fn sanitize_quarantine_reason(reason: &str) -> String {
    let sanitized: String = reason
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    sanitized
        .trim_matches('-')
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn quarantine_db_file(path: &Path, reason: &str) -> std::io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.redb");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let reason = sanitize_quarantine_reason(reason);

    for attempt in 0..100u32 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let candidate = parent.join(format!(
            "{file_name}.quarantine.{timestamp}.{reason}{suffix}"
        ));
        if !candidate.exists() {
            std::fs::rename(path, &candidate)?;
            return Ok(candidate);
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("failed to quarantine {} after 100 attempts", path.display()),
    ))
}

pub async fn startup_integrity_check(
    store: &Store,
    nats_client: &NatsClient,
    storage_config: &StorageSection,
) {
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
            let db_path = store.db_path();
            tracing::error!(path = %db_path.display(), "critical tables corrupted — full re-bootstrap required");
            match storage_config.integrity_failure_mode {
                StorageIntegrityFailureMode::QuarantineAndExit => {
                    match quarantine_db_file(&db_path, "integrity_full_rebootstrap") {
                        Ok(quarantined_path) => {
                            tracing::error!(
                                path = %db_path.display(),
                                quarantined_path = %quarantined_path.display(),
                                "corrupted redb quarantined; restart the node to bootstrap from cluster state"
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                path = %db_path.display(),
                                "failed to quarantine corrupted redb — manual intervention required"
                            );
                        }
                    }
                }
                StorageIntegrityFailureMode::DeleteAndExit => {
                    match std::fs::remove_file(&db_path) {
                        Ok(_) => {
                            tracing::error!(
                                path = %db_path.display(),
                                "corrupted redb deleted due to explicit destructive recovery mode; restart required for clean bootstrap"
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                path = %db_path.display(),
                                "failed to delete corrupted redb in destructive recovery mode"
                            );
                        }
                    }
                }
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::quarantine_db_file;

    #[test]
    fn test_quarantine_db_file_moves_db_with_reason_suffix() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("state.redb");
        std::fs::write(&db_path, b"corrupted").unwrap();

        let quarantined = quarantine_db_file(&db_path, "open failure!").unwrap();

        assert!(!db_path.exists());
        assert!(quarantined.exists());
        let file_name = quarantined.file_name().unwrap().to_string_lossy();
        assert!(file_name.contains("state.redb.quarantine."));
        assert!(file_name.contains("open-failure"));
    }
}
