// crates/node/src/upgrade.rs
use common::error::PlatformError;
use common::protocol::{MIN_COMPATIBLE_PROTOCOL, PROTOCOL_VERSION};
use common::upgrade_provenance::SignedNodeBinaryReleaseProvenance;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use messaging::events::{node_upgrade_signature_payload, Event};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn staged_binary_path(install_dir: &Path, binary_name: &str, expected_sha256: &str) -> PathBuf {
    install_dir.join(format!(
        ".{binary_name}.{}.download",
        &expected_sha256[..12]
    ))
}

fn final_binary_path(install_dir: &Path, binary_name: &str, expected_sha256: &str) -> PathBuf {
    install_dir.join(format!("{}-{}", binary_name, &expected_sha256[..12]))
}

fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
}

fn update_release_links(install_dir: &Path, new_binary_path: &Path) -> Result<(), PlatformError> {
    let current_link = install_dir.join("current");
    let previous_link = install_dir.join("previous");
    let temp_current = install_dir.join("current.next");

    let previous_target = std::fs::read_link(&current_link).ok();
    if let Some(previous_target) = previous_target {
        let _ = std::fs::remove_file(&previous_link);
        create_symlink(&previous_target, &previous_link)
            .map_err(|e| PlatformError::storage_with_msg("failed to create previous symlink", e))?;
    }

    let _ = std::fs::remove_file(&temp_current);
    create_symlink(new_binary_path, &temp_current).map_err(|e| {
        PlatformError::storage_with_msg("failed to create staged current symlink", e)
    })?;

    if current_link.exists() {
        std::fs::remove_file(&current_link).map_err(|e| {
            PlatformError::storage_with_msg("failed to remove old current symlink", e)
        })?;
    }
    std::fs::rename(&temp_current, &current_link)
        .map_err(|e| PlatformError::storage_with_msg("failed to activate current symlink", e))?;

    Ok(())
}

fn parse_verifying_key_hex(key_hex: &str) -> Result<VerifyingKey, PlatformError> {
    let bytes = hex::decode(key_hex).map_err(|e| {
        PlatformError::config_validation(format!("invalid upgrade signing public key hex: {e}"))
    })?;
    let len = bytes.len();
    let key_bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        PlatformError::config_validation(format!(
            "upgrade signing public key must be 32 bytes, got {} bytes",
            len
        ))
    })?;
    VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| PlatformError::security(format!("invalid Ed25519 public key: {e}")))
}

fn parse_signature_hex(signature_hex: &str) -> Result<Signature, PlatformError> {
    let bytes = hex::decode(signature_hex)
        .map_err(|e| PlatformError::security(format!("invalid upgrade signature hex: {e}")))?;
    let len = bytes.len();
    let sig_bytes: [u8; 64] = bytes.try_into().map_err(|_| {
        PlatformError::security(format!(
            "upgrade signature must be 64 bytes, got {} bytes",
            len
        ))
    })?;
    Ok(Signature::from_bytes(&sig_bytes))
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn verify_upgrade_signature(
    event: &Event,
    configured_public_key_hex: Option<&str>,
) -> Result<(), PlatformError> {
    let Some(public_key_hex) = configured_public_key_hex else {
        return Ok(());
    };

    let (
        target_node,
        binary_url,
        binary_sha256,
        signature_ed25519,
        release_provenance,
        new_protocol_version,
        new_binary_version,
    ) = match event {
        Event::NodeUpgrade {
            target_node,
            binary_url,
            binary_sha256,
            signature_ed25519,
            release_provenance,
            new_protocol_version,
            new_binary_version,
        } => (
            target_node,
            binary_url,
            binary_sha256,
            signature_ed25519,
            release_provenance,
            *new_protocol_version,
            new_binary_version,
        ),
        _ => return Ok(()),
    };

    if let Some(provenance) = release_provenance.as_ref() {
        return verify_release_provenance(
            provenance,
            public_key_hex,
            binary_url,
            binary_sha256,
            new_protocol_version,
            new_binary_version,
            now_unix_ms(),
        )
        .map_err(PlatformError::security);
    }

    let signature_hex = signature_ed25519.as_deref().ok_or_else(|| {
        PlatformError::security(
            "upgrade signature missing while runtime.upgrade_signing_public_key is configured"
                .to_string(),
        )
    })?;

    let verifying_key = parse_verifying_key_hex(public_key_hex)?;
    let signature = parse_signature_hex(signature_hex)?;
    let payload = node_upgrade_signature_payload(
        target_node,
        binary_url,
        binary_sha256,
        new_protocol_version,
        new_binary_version,
    );

    verifying_key
        .verify(&payload, &signature)
        .map_err(|e| PlatformError::security(format!("upgrade signature verification failed: {e}")))
}

fn verify_release_provenance(
    provenance: &SignedNodeBinaryReleaseProvenance,
    configured_public_key_hex: &str,
    binary_url: &str,
    binary_sha256: &str,
    new_protocol_version: u32,
    new_binary_version: &str,
    now_ms: u64,
) -> Result<(), String> {
    provenance.verify(
        configured_public_key_hex,
        binary_url,
        binary_sha256,
        new_protocol_version,
        new_binary_version,
        now_ms,
    )
}

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

/// Download the new binary, verify its hash, stage it to a temporary file,
/// atomically rename it into place, and update current/previous release links.
pub async fn download_and_verify(
    artifact_url: &str,
    expected_sha256: &str,
    install_dir: &Path,
    binary_name: &str,
) -> Result<PathBuf, PlatformError> {
    let bytes = download_and_verify_bytes(artifact_url, expected_sha256).await?;

    tokio::fs::create_dir_all(install_dir)
        .await
        .map_err(|e| PlatformError::storage_with_msg("create install dir failed", e))?;

    let staged = staged_binary_path(install_dir, binary_name, expected_sha256);
    let dest = final_binary_path(install_dir, binary_name, expected_sha256);

    tokio::fs::write(&staged, &bytes)
        .await
        .map_err(|e| PlatformError::storage_with_msg("write staged binary failed", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&staged, perms)
            .map_err(|e| PlatformError::storage_with_msg("chmod staged binary failed", e))?;
    }

    if !dest.exists() {
        std::fs::rename(&staged, &dest)
            .map_err(|e| PlatformError::storage_with_msg("activate binary failed", e))?;
    } else {
        let _ = tokio::fs::remove_file(&staged).await;
    }

    update_release_links(install_dir, &dest)?;

    tracing::info!(path = %dest.display(), "binary installed and activated");
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
    use common::upgrade_provenance::{
        NodeBinaryReleaseProvenance, ReleaseKeyDelegation, SignedNodeBinaryReleaseProvenance,
        SignedReleaseKeyDelegation, UPGRADE_PROVENANCE_SCOPE_NODE_BINARY,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use messaging::events::{node_upgrade_signature_payload, Event};
    use tempfile::TempDir;

    const TEST_UPGRADE_SIGNING_KEY_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn test_upgrade_action_not_targeted() {
        let event = Event::NodeUpgrade {
            target_node: "node-1".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            signature_ed25519: None,
            release_provenance: None,
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
            signature_ed25519: None,
            release_provenance: None,
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
            signature_ed25519: None,
            release_provenance: None,
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
            signature_ed25519: None,
            release_provenance: None,
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
            signature_ed25519: None,
            release_provenance: None,
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
            signature_ed25519: None,
            release_provenance: None,
            new_protocol_version: PROTOCOL_VERSION + 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let action = handle_upgrade_event(&event, "node-0", &["node-0".to_string()]).unwrap();
        assert_eq!(action, UpgradeAction::ProceedWithUpgrade);
    }

    #[test]
    fn test_verify_upgrade_signature_accepts_valid_signature() {
        let key_bytes = hex::decode(TEST_UPGRADE_SIGNING_KEY_HEX).unwrap();
        let signing_key = SigningKey::from_bytes(&key_bytes.try_into().unwrap());
        let verifying_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let payload = node_upgrade_signature_payload(
            "node-0",
            "http://example.com/binary",
            "abc123",
            1,
            "0.2.0",
        );
        let signature = hex::encode(signing_key.sign(&payload).to_bytes());

        let event = Event::NodeUpgrade {
            target_node: "node-0".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            signature_ed25519: Some(signature),
            release_provenance: None,
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        verify_upgrade_signature(&event, Some(&verifying_key_hex)).unwrap();
    }

    #[test]
    fn test_verify_upgrade_signature_rejects_missing_signature_when_key_configured() {
        let key_bytes = hex::decode(TEST_UPGRADE_SIGNING_KEY_HEX).unwrap();
        let signing_key = SigningKey::from_bytes(&key_bytes.try_into().unwrap());
        let verifying_key_hex = hex::encode(signing_key.verifying_key().to_bytes());

        let event = Event::NodeUpgrade {
            target_node: "node-0".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            signature_ed25519: None,
            release_provenance: None,
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let err = verify_upgrade_signature(&event, Some(&verifying_key_hex)).unwrap_err();
        assert!(err.to_string().contains("upgrade signature missing"));
    }

    #[test]
    fn test_verify_upgrade_signature_rejects_bad_signature() {
        let key_bytes = hex::decode(TEST_UPGRADE_SIGNING_KEY_HEX).unwrap();
        let signing_key = SigningKey::from_bytes(&key_bytes.try_into().unwrap());
        let verifying_key_hex = hex::encode(signing_key.verifying_key().to_bytes());

        let event = Event::NodeUpgrade {
            target_node: "node-0".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            signature_ed25519: Some("00".repeat(64)),
            release_provenance: None,
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let err = verify_upgrade_signature(&event, Some(&verifying_key_hex)).unwrap_err();
        assert!(
            err.to_string().contains("verification failed") || err.to_string().contains("invalid")
        );
    }

    fn make_signed_release_provenance(
        binary_url: &str,
        binary_sha256: &str,
        protocol_version: u32,
        binary_version: &str,
    ) -> (String, SignedNodeBinaryReleaseProvenance) {
        let root = SigningKey::from_bytes(
            &hex::decode(TEST_UPGRADE_SIGNING_KEY_HEX)
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let leaf = SigningKey::from_bytes(&[0x22; 32]);
        let delegation = SignedReleaseKeyDelegation::sign(
            ReleaseKeyDelegation {
                version: 1,
                key_id: "release-key-1".to_string(),
                public_key_ed25519: hex::encode(leaf.verifying_key().to_bytes()),
                scope: UPGRADE_PROVENANCE_SCOPE_NODE_BINARY.to_string(),
                issuer: "release-root".to_string(),
                issued_at_ms: now_unix_ms().saturating_sub(1_000),
                expires_at_ms: now_unix_ms().saturating_add(60_000),
            },
            &root,
        );
        let provenance = SignedNodeBinaryReleaseProvenance::sign(
            NodeBinaryReleaseProvenance {
                version: 1,
                delegation_key_id: "release-key-1".to_string(),
                binary_url: binary_url.to_string(),
                binary_sha256: binary_sha256.to_string(),
                new_protocol_version: protocol_version,
                new_binary_version: binary_version.to_string(),
                source_repository: Some("https://github.com/example/repo".to_string()),
                source_commit_sha: Some("abc123".to_string()),
                build_workflow_ref: Some("release.yml".to_string()),
                build_run_id: Some("42".to_string()),
                issued_at_ms: now_unix_ms().saturating_sub(500),
                expires_at_ms: now_unix_ms().saturating_add(60_000),
            },
            delegation,
            &leaf,
        );
        (hex::encode(root.verifying_key().to_bytes()), provenance)
    }

    #[test]
    fn test_verify_upgrade_signature_accepts_release_provenance_bundle() {
        let (verifying_key_hex, release_provenance) =
            make_signed_release_provenance("http://example.com/binary", "abc123", 1, "0.2.0");

        let event = Event::NodeUpgrade {
            target_node: "node-0".to_string(),
            binary_url: "http://example.com/binary".to_string(),
            binary_sha256: "abc123".to_string(),
            signature_ed25519: None,
            release_provenance: Some(release_provenance),
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        verify_upgrade_signature(&event, Some(&verifying_key_hex)).unwrap();
    }

    #[test]
    fn test_verify_upgrade_signature_rejects_mismatched_release_provenance() {
        let (verifying_key_hex, release_provenance) =
            make_signed_release_provenance("http://example.com/binary", "abc123", 1, "0.2.0");

        let event = Event::NodeUpgrade {
            target_node: "node-0".to_string(),
            binary_url: "http://example.com/other-binary".to_string(),
            binary_sha256: "abc123".to_string(),
            signature_ed25519: None,
            release_provenance: Some(release_provenance),
            new_protocol_version: 1,
            new_binary_version: "0.2.0".to_string(),
        };

        let err = verify_upgrade_signature(&event, Some(&verifying_key_hex)).unwrap_err();
        assert!(err.to_string().contains("binary_url mismatch"));
    }

    #[test]
    fn test_final_binary_path_uses_hash_suffix() {
        let install_dir = Path::new("/opt/wasm-cloud");
        let path = final_binary_path(
            install_dir,
            "node",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );
        assert_eq!(path, install_dir.join("node-0123456789ab"));
    }

    #[test]
    fn test_update_release_links_preserves_previous_target() {
        let temp_dir = TempDir::new().unwrap();
        let install_dir = temp_dir.path();
        let old_binary = install_dir.join("node-old");
        let new_binary = install_dir.join("node-new");
        std::fs::write(&old_binary, b"old").unwrap();
        std::fs::write(&new_binary, b"new").unwrap();

        let current_link = install_dir.join("current");
        create_symlink(&old_binary, &current_link).unwrap();

        update_release_links(install_dir, &new_binary).unwrap();

        let current_target = std::fs::read_link(install_dir.join("current")).unwrap();
        let previous_target = std::fs::read_link(install_dir.join("previous")).unwrap();
        assert_eq!(current_target, new_binary);
        assert_eq!(previous_target, old_binary);
    }
}
