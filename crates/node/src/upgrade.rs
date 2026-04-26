// crates/node/src/upgrade.rs
use common::error::PlatformError;
use common::protocol::{MIN_COMPATIBLE_PROTOCOL, PROTOCOL_VERSION};
use messaging::events::Event;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Download the new binary, verify its hash, and write it to disk.
/// Download an artifact and verify its SHA-256 hash, returning the raw bytes.
///
/// This is the core download+verify logic shared by both artifact fetching
/// (for Wasm modules) and binary upgrades.
pub async fn download_and_verify_bytes(
    artifact_url: &str,
    expected_sha256: &str,
) -> Result<Vec<u8>, PlatformError> {
    tracing::info!(url = %artifact_url, "downloading artifact");

    // 1. Download
    let response = reqwest::get(artifact_url)
        .await
        .map_err(|e| PlatformError::network(format!("download failed: {}", e)))?;

    if !response.status().is_success() {
        return Err(PlatformError::network(format!(
            "download failed with status: {}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| PlatformError::network(format!("read body failed: {}", e)))?;

    tracing::info!(bytes = bytes.len(), "artifact downloaded");

    // 2. Verify SHA-256
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual_hash = hex::encode(hasher.finalize());

    if actual_hash != expected_sha256 {
        return Err(PlatformError::Security(format!(
            "SHA-256 mismatch: expected {}, got {}. Aborting.",
            expected_sha256, actual_hash
        )));
    }

    tracing::info!(sha256 = %actual_hash, "artifact hash verified");

    Ok(bytes.to_vec())
}

/// Download the new binary, verify its hash, and write it to disk.
pub async fn download_and_verify(
    artifact_url: &str,
    expected_sha256: &str,
    install_dir: &Path,
    binary_name: &str,
) -> Result<PathBuf, PlatformError> {
    let bytes = download_and_verify_bytes(artifact_url, expected_sha256).await?;

    // 3. Ensure install directory exists
    tokio::fs::create_dir_all(install_dir)
        .await
        .map_err(|e| PlatformError::storage_with_msg("create install dir failed", e))?;

    // 4. Write to install directory
    let dest = install_dir.join(binary_name);
    tokio::fs::write(&dest, &bytes)
        .await
        .map_err(|e| PlatformError::storage_with_msg("write binary failed", e))?;

    // 5. Set executable permission (Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&dest, perms)
            .map_err(|e| PlatformError::storage_with_msg("chmod failed", e))?;
    }

    tracing::info!(path = %dest.display(), "binary installed");
    Ok(dest)
}

/// Determine what action to take for an upgrade event.
pub fn handle_upgrade_event(
    event: &Event,
    own_node_id: &str,
    cluster_node_ids: &[String],
) -> Result<UpgradeAction, PlatformError> {
    let (target_node, new_protocol_version) = match event {
        Event::NodeUpgrade {
            target_node,
            new_protocol_version,
            ..
        } => (target_node, *new_protocol_version),
        _ => return Ok(UpgradeAction::NotAnUpgradeEvent),
    };

    // Check if this event targets us
    if target_node != "*" && target_node != own_node_id {
        return Ok(UpgradeAction::NotTargeted);
    }

    // For rolling upgrades (target = "*"), check if it's our turn
    if target_node == "*" {
        let mut sorted_nodes = cluster_node_ids.to_vec();
        sorted_nodes.sort();

        let my_position = sorted_nodes
            .iter()
            .position(|id| id == own_node_id)
            .ok_or_else(|| PlatformError::Internal("own node not in cluster list".to_string()))?;

        if my_position > 0 {
            // Wait for the previous node to confirm its upgrade
            let predecessor = sorted_nodes[my_position - 1].clone();
            tracing::info!(
                waiting_for = %predecessor,
                position = my_position,
                "waiting for previous node to complete upgrade"
            );
            return Ok(UpgradeAction::WaitForPredecessor { predecessor });
        }
    }

    // Protocol compatibility check
    if new_protocol_version < MIN_COMPATIBLE_PROTOCOL {
        tracing::error!(
            new = new_protocol_version,
            min_supported = MIN_COMPATIBLE_PROTOCOL,
            "new protocol version too old"
        );
        return Ok(UpgradeAction::IncompatibleVersion);
    }

    if new_protocol_version > PROTOCOL_VERSION + 1 {
        tracing::error!(
            current = PROTOCOL_VERSION,
            new = new_protocol_version,
            "protocol version gap too large — upgrade intermediate version first"
        );
        return Ok(UpgradeAction::IncompatibleVersion);
    }

    Ok(UpgradeAction::ProceedWithUpgrade)
}

#[derive(Debug, PartialEq, Eq)]
pub enum UpgradeAction {
    NotAnUpgradeEvent,
    NotTargeted,
    WaitForPredecessor { predecessor: String },
    IncompatibleVersion,
    ProceedWithUpgrade,
}

#[cfg(test)]
mod tests {
    use super::*;
    use messaging::events::Event;

    #[test]
    fn test_upgrade_action_not_targeted() {
        let event = Event::NodeUpgrade {
            target_node: "node-1".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let action = handle_upgrade_event(&event, "node-0", &["node-0".to_string()]).unwrap();
        assert_eq!(action, UpgradeAction::NotTargeted);
    }

    #[test]
    fn test_upgrade_action_proceed_single_target() {
        let event = Event::NodeUpgrade {
            target_node: "node-0".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let action = handle_upgrade_event(&event, "node-0", &["node-0".to_string()]).unwrap();
        assert_eq!(action, UpgradeAction::ProceedWithUpgrade);
    }

    #[test]
    fn test_upgrade_action_rolling_first_node() {
        let event = Event::NodeUpgrade {
            target_node: "*".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let cluster = vec![
            "node-0".to_string(),
            "node-1".to_string(),
            "node-2".to_string(),
        ];

        // First node should proceed
        let action = handle_upgrade_event(&event, "node-0", &cluster).unwrap();
        assert_eq!(action, UpgradeAction::ProceedWithUpgrade);
    }

    #[test]
    fn test_upgrade_action_rolling_wait_for_predecessor() {
        let event = Event::NodeUpgrade {
            target_node: "*".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let cluster = vec![
            "node-0".to_string(),
            "node-1".to_string(),
            "node-2".to_string(),
        ];

        // Second node should wait
        let action = handle_upgrade_event(&event, "node-1", &cluster).unwrap();
        assert_eq!(
            action,
            UpgradeAction::WaitForPredecessor {
                predecessor: "node-0".to_string()
            }
        );

        // Third node should wait
        let action = handle_upgrade_event(&event, "node-2", &cluster).unwrap();
        assert_eq!(
            action,
            UpgradeAction::WaitForPredecessor {
                predecessor: "node-1".to_string()
            }
        );
    }

    #[test]
    fn test_upgrade_action_incompatible_version_too_new() {
        let event = Event::NodeUpgrade {
            target_node: "node-0".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            new_protocol_version: PROTOCOL_VERSION + 2,
            new_binary_version: "0.2.0".to_string(),
        };

        let action = handle_upgrade_event(&event, "node-0", &["node-0".to_string()]).unwrap();
        assert_eq!(action, UpgradeAction::IncompatibleVersion);
    }

    #[test]
    fn test_upgrade_action_compatible_one_ahead() {
        let event = Event::NodeUpgrade {
            target_node: "node-0".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            new_protocol_version: PROTOCOL_VERSION + 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let action = handle_upgrade_event(&event, "node-0", &["node-0".to_string()]).unwrap();
        assert_eq!(action, UpgradeAction::ProceedWithUpgrade);
    }
}
