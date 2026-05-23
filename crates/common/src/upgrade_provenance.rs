use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

pub const UPGRADE_PROVENANCE_SCOPE_NODE_BINARY: &str = "node-binary-upgrade";

fn default_version() -> u8 {
    1
}

fn parse_verifying_key_hex(key_hex: &str) -> Result<VerifyingKey, String> {
    let bytes = hex::decode(key_hex.trim())
        .map_err(|e| format!("invalid provenance public key hex: {e}"))?;
    let len = bytes.len();
    let key_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| format!("provenance public key must be 32 bytes, got {len} bytes"))?;
    VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| format!("invalid Ed25519 provenance public key: {e}"))
}

fn parse_signature_hex(signature_hex: &str) -> Result<Signature, String> {
    let bytes = hex::decode(signature_hex.trim())
        .map_err(|e| format!("invalid provenance signature hex: {e}"))?;
    let len = bytes.len();
    let sig_bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| format!("provenance signature must be 64 bytes, got {len} bytes"))?;
    Ok(Signature::from_bytes(&sig_bytes))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseKeyDelegation {
    #[serde(default = "default_version")]
    pub version: u8,
    pub key_id: String,
    pub public_key_ed25519: String,
    pub scope: String,
    pub issuer: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

impl ReleaseKeyDelegation {
    pub fn canonical_payload(&self) -> Vec<u8> {
        format!(
            "version={}\nkey_id={}\npublic_key_ed25519={}\nscope={}\nissuer={}\nissued_at_ms={}\nexpires_at_ms={}\n",
            self.version,
            self.key_id,
            self.public_key_ed25519,
            self.scope,
            self.issuer,
            self.issued_at_ms,
            self.expires_at_ms,
        )
        .into_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedReleaseKeyDelegation {
    pub delegation: ReleaseKeyDelegation,
    pub signature_ed25519: String,
}

impl SignedReleaseKeyDelegation {
    pub fn sign(delegation: ReleaseKeyDelegation, signing_key: &SigningKey) -> Self {
        let signature = signing_key.sign(&delegation.canonical_payload());
        Self {
            delegation,
            signature_ed25519: hex::encode(signature.to_bytes()),
        }
    }

    pub fn verify(&self, verifying_key: &VerifyingKey) -> Result<(), String> {
        let signature = parse_signature_hex(&self.signature_ed25519)?;
        verifying_key
            .verify(&self.delegation.canonical_payload(), &signature)
            .map_err(|e| format!("release key delegation verification failed: {e}"))
    }

    pub fn delegated_verifying_key(&self) -> Result<VerifyingKey, String> {
        parse_verifying_key_hex(&self.delegation.public_key_ed25519)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeBinaryReleaseProvenance {
    #[serde(default = "default_version")]
    pub version: u8,
    pub delegation_key_id: String,
    pub binary_url: String,
    pub binary_sha256: String,
    pub new_protocol_version: u32,
    pub new_binary_version: String,
    #[serde(default)]
    pub source_repository: Option<String>,
    #[serde(default)]
    pub source_commit_sha: Option<String>,
    #[serde(default)]
    pub build_workflow_ref: Option<String>,
    #[serde(default)]
    pub build_run_id: Option<String>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

impl NodeBinaryReleaseProvenance {
    pub fn canonical_payload(&self) -> Vec<u8> {
        format!(
            "version={}\ndelegation_key_id={}\nbinary_url={}\nbinary_sha256={}\nnew_protocol_version={}\nnew_binary_version={}\nsource_repository={}\nsource_commit_sha={}\nbuild_workflow_ref={}\nbuild_run_id={}\nissued_at_ms={}\nexpires_at_ms={}\n",
            self.version,
            self.delegation_key_id,
            self.binary_url,
            self.binary_sha256,
            self.new_protocol_version,
            self.new_binary_version,
            self.source_repository.as_deref().unwrap_or(""),
            self.source_commit_sha.as_deref().unwrap_or(""),
            self.build_workflow_ref.as_deref().unwrap_or(""),
            self.build_run_id.as_deref().unwrap_or(""),
            self.issued_at_ms,
            self.expires_at_ms,
        )
        .into_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedNodeBinaryReleaseProvenance {
    pub provenance: NodeBinaryReleaseProvenance,
    pub delegation: SignedReleaseKeyDelegation,
    pub signature_ed25519: String,
}

impl SignedNodeBinaryReleaseProvenance {
    pub fn sign(
        provenance: NodeBinaryReleaseProvenance,
        delegation: SignedReleaseKeyDelegation,
        signing_key: &SigningKey,
    ) -> Self {
        let signature = signing_key.sign(&provenance.canonical_payload());
        Self {
            provenance,
            delegation,
            signature_ed25519: hex::encode(signature.to_bytes()),
        }
    }

    pub fn verify(
        &self,
        root_verifying_key_hex: &str,
        expected_binary_url: &str,
        expected_binary_sha256: &str,
        expected_protocol_version: u32,
        expected_binary_version: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        let root_verifying_key = parse_verifying_key_hex(root_verifying_key_hex)?;
        self.delegation.verify(&root_verifying_key)?;

        if self.delegation.delegation.version != default_version() {
            return Err(format!(
                "unsupported delegation version {}",
                self.delegation.delegation.version
            ));
        }
        if self.delegation.delegation.scope != UPGRADE_PROVENANCE_SCOPE_NODE_BINARY {
            return Err(format!(
                "delegation scope mismatch: expected {}, got {}",
                UPGRADE_PROVENANCE_SCOPE_NODE_BINARY, self.delegation.delegation.scope
            ));
        }
        if self.delegation.delegation.expires_at_ms <= self.delegation.delegation.issued_at_ms {
            return Err("delegation expiry must be after issued_at".to_string());
        }
        if now_ms >= self.delegation.delegation.expires_at_ms {
            return Err(format!(
                "delegation expired at {}",
                self.delegation.delegation.expires_at_ms
            ));
        }
        if self.provenance.delegation_key_id != self.delegation.delegation.key_id {
            return Err(format!(
                "provenance delegation key mismatch: expected {}, got {}",
                self.delegation.delegation.key_id, self.provenance.delegation_key_id
            ));
        }

        let delegated_verifying_key = self.delegation.delegated_verifying_key()?;
        let signature = parse_signature_hex(&self.signature_ed25519)?;
        delegated_verifying_key
            .verify(&self.provenance.canonical_payload(), &signature)
            .map_err(|e| format!("release provenance verification failed: {e}"))?;

        if self.provenance.version != default_version() {
            return Err(format!(
                "unsupported release provenance version {}",
                self.provenance.version
            ));
        }
        if self.provenance.binary_url != expected_binary_url {
            return Err(format!(
                "release provenance binary_url mismatch: expected {}, got {}",
                expected_binary_url, self.provenance.binary_url
            ));
        }
        if self.provenance.binary_sha256 != expected_binary_sha256 {
            return Err(format!(
                "release provenance binary_sha256 mismatch: expected {}, got {}",
                expected_binary_sha256, self.provenance.binary_sha256
            ));
        }
        if self.provenance.new_protocol_version != expected_protocol_version {
            return Err(format!(
                "release provenance protocol version mismatch: expected {}, got {}",
                expected_protocol_version, self.provenance.new_protocol_version
            ));
        }
        if self.provenance.new_binary_version != expected_binary_version {
            return Err(format!(
                "release provenance binary version mismatch: expected {}, got {}",
                expected_binary_version, self.provenance.new_binary_version
            ));
        }
        if self.provenance.expires_at_ms <= self.provenance.issued_at_ms {
            return Err("release provenance expiry must be after issued_at".to_string());
        }
        if now_ms >= self.provenance.expires_at_ms {
            return Err(format!(
                "release provenance expired at {}",
                self.provenance.expires_at_ms
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signing_key_from_hex(hex_key: &str) -> SigningKey {
        let bytes = hex::decode(hex_key).unwrap();
        SigningKey::from_bytes(&bytes.try_into().unwrap())
    }

    #[test]
    fn test_signed_release_provenance_verifies_with_root_signed_delegation() {
        let root = signing_key_from_hex(
            "1111111111111111111111111111111111111111111111111111111111111111",
        );
        let leaf = signing_key_from_hex(
            "2222222222222222222222222222222222222222222222222222222222222222",
        );
        let delegation = SignedReleaseKeyDelegation::sign(
            ReleaseKeyDelegation {
                version: 1,
                key_id: "release-key-1".to_string(),
                public_key_ed25519: hex::encode(leaf.verifying_key().to_bytes()),
                scope: UPGRADE_PROVENANCE_SCOPE_NODE_BINARY.to_string(),
                issuer: "release-root".to_string(),
                issued_at_ms: 1_000,
                expires_at_ms: 10_000,
            },
            &root,
        );
        let signed = SignedNodeBinaryReleaseProvenance::sign(
            NodeBinaryReleaseProvenance {
                version: 1,
                delegation_key_id: "release-key-1".to_string(),
                binary_url: "https://example.com/node".to_string(),
                binary_sha256: "deadbeef".to_string(),
                new_protocol_version: 1,
                new_binary_version: "1.2.3".to_string(),
                source_repository: Some("https://github.com/example/repo".to_string()),
                source_commit_sha: Some("abc123".to_string()),
                build_workflow_ref: Some("release.yml".to_string()),
                build_run_id: Some("42".to_string()),
                issued_at_ms: 2_000,
                expires_at_ms: 9_000,
            },
            delegation,
            &leaf,
        );

        signed
            .verify(
                &hex::encode(root.verifying_key().to_bytes()),
                "https://example.com/node",
                "deadbeef",
                1,
                "1.2.3",
                3_000,
            )
            .unwrap();
    }
}
