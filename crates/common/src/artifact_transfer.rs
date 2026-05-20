use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const ARTIFACT_TRANSFER_MANIFEST_HEADER: &str = "x-wasm-artifact-transfer-manifest";
pub const DEFAULT_ARTIFACT_GET_MANIFEST_TTL_SECS: u64 = 60;
pub const DEFAULT_ARTIFACT_PUT_MANIFEST_TTL_SECS: u64 = 60;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ArtifactTransferMethod {
    Get,
    Put,
}

impl ArtifactTransferMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactTransferMethod::Get => "GET",
            ArtifactTransferMethod::Put => "PUT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactTransferManifest {
    #[serde(default = "default_manifest_version")]
    pub version: u8,
    pub artifact_sha256: String,
    pub artifact_path: String,
    pub method: ArtifactTransferMethod,
    pub issuer: String,
    #[serde(default)]
    pub audience: Option<String>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub transfer_id: String,
    #[serde(default)]
    pub single_use: bool,
}

fn default_manifest_version() -> u8 {
    1
}

impl ArtifactTransferManifest {
    pub fn canonical_payload(&self) -> Vec<u8> {
        format!(
            "version={}\nartifact_sha256={}\nartifact_path={}\nmethod={}\nissuer={}\naudience={}\nissued_at_ms={}\nexpires_at_ms={}\ntransfer_id={}\nsingle_use={}\n",
            self.version,
            self.artifact_sha256,
            self.artifact_path,
            self.method.as_str(),
            self.issuer,
            self.audience.as_deref().unwrap_or(""),
            self.issued_at_ms,
            self.expires_at_ms,
            self.transfer_id,
            self.single_use,
        )
        .into_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedArtifactTransferManifest {
    pub manifest: ArtifactTransferManifest,
    pub signature_ed25519: String,
}

impl SignedArtifactTransferManifest {
    pub fn sign(manifest: ArtifactTransferManifest, signing_key: &SigningKey) -> Self {
        let signature = signing_key.sign(&manifest.canonical_payload());
        SignedArtifactTransferManifest {
            manifest,
            signature_ed25519: hex::encode(signature.to_bytes()),
        }
    }

    pub fn verify(&self, verifying_key: &VerifyingKey) -> Result<(), String> {
        let signature_bytes = hex::decode(self.signature_ed25519.trim())
            .map_err(|e| format!("manifest signature is not valid hex: {e}"))?;
        let signature = Signature::try_from(signature_bytes.as_slice())
            .map_err(|e| format!("manifest signature has invalid length/format: {e}"))?;
        verifying_key
            .verify(&self.manifest.canonical_payload(), &signature)
            .map_err(|e| format!("manifest signature verification failed: {e}"))
    }

    pub fn encode_header_value(&self) -> Result<String, String> {
        serde_json::to_vec(self)
            .map(hex::encode)
            .map_err(|e| format!("failed to encode manifest header: {e}"))
    }

    pub fn decode_header_value(value: &str) -> Result<Self, String> {
        let raw = hex::decode(value.trim())
            .map_err(|e| format!("manifest header is not valid hex: {e}"))?;
        serde_json::from_slice(&raw).map_err(|e| format!("manifest header is not valid JSON: {e}"))
    }
}

#[derive(Clone)]
pub struct ArtifactTransferAuthority {
    local_node_id: String,
    signing_seed: [u8; 32],
}

impl std::fmt::Debug for ArtifactTransferAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactTransferAuthority")
            .field("local_node_id", &self.local_node_id)
            .finish_non_exhaustive()
    }
}

impl ArtifactTransferAuthority {
    pub fn derive(local_node_id: &str, key_material: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"wasm-cloud-platform:artifact-transfer:ed25519:v1\n");
        hasher.update(local_node_id.as_bytes());
        hasher.update(b"\n");
        hasher.update(key_material);
        let digest = hasher.finalize();
        let mut signing_seed = [0u8; 32];
        signing_seed.copy_from_slice(&digest[..32]);
        ArtifactTransferAuthority {
            local_node_id: local_node_id.to_string(),
            signing_seed,
        }
    }

    pub fn local_node_id(&self) -> &str {
        &self.local_node_id
    }

    pub fn issue_manifest(
        &self,
        artifact_sha256: &str,
        method: ArtifactTransferMethod,
        ttl_secs: u64,
        single_use: bool,
    ) -> SignedArtifactTransferManifest {
        let now_ms = now_unix_ms();
        let manifest = ArtifactTransferManifest {
            version: default_manifest_version(),
            artifact_sha256: artifact_sha256.to_string(),
            artifact_path: format!("/artifacts/{artifact_sha256}"),
            method,
            issuer: self.local_node_id.clone(),
            audience: Some(self.local_node_id.clone()),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl_secs.saturating_mul(1000)),
            transfer_id: Uuid::new_v4().to_string(),
            single_use,
        };
        SignedArtifactTransferManifest::sign(manifest, &self.signing_key())
    }

    pub fn issue_read_manifest(&self, artifact_sha256: &str) -> SignedArtifactTransferManifest {
        self.issue_manifest(
            artifact_sha256,
            ArtifactTransferMethod::Get,
            DEFAULT_ARTIFACT_GET_MANIFEST_TTL_SECS,
            false,
        )
    }

    pub fn issue_write_manifest(&self, artifact_sha256: &str) -> SignedArtifactTransferManifest {
        self.issue_manifest(
            artifact_sha256,
            ArtifactTransferMethod::Put,
            DEFAULT_ARTIFACT_PUT_MANIFEST_TTL_SECS,
            true,
        )
    }

    pub fn verify_manifest(
        &self,
        signed: &SignedArtifactTransferManifest,
        expected_sha256: &str,
        expected_method: ArtifactTransferMethod,
        now_ms: u64,
    ) -> Result<(), String> {
        signed.verify(&self.verifying_key())?;

        if signed.manifest.version != default_manifest_version() {
            return Err(format!(
                "unsupported manifest version {}",
                signed.manifest.version
            ));
        }
        if signed.manifest.method != expected_method {
            return Err(format!(
                "manifest method mismatch: expected {}, got {}",
                expected_method.as_str(),
                signed.manifest.method.as_str()
            ));
        }
        if signed.manifest.artifact_sha256 != expected_sha256 {
            return Err(format!(
                "manifest artifact digest mismatch: expected {}, got {}",
                expected_sha256, signed.manifest.artifact_sha256
            ));
        }
        if signed.manifest.artifact_path != format!("/artifacts/{expected_sha256}") {
            return Err(format!(
                "manifest artifact path mismatch: expected /artifacts/{expected_sha256}, got {}",
                signed.manifest.artifact_path
            ));
        }
        if signed.manifest.issuer != self.local_node_id {
            return Err(format!(
                "manifest issuer mismatch: expected {}, got {}",
                self.local_node_id, signed.manifest.issuer
            ));
        }
        if signed.manifest.audience.as_deref() != Some(self.local_node_id.as_str()) {
            return Err(format!(
                "manifest audience mismatch: expected {}, got {:?}",
                self.local_node_id, signed.manifest.audience
            ));
        }
        if signed.manifest.expires_at_ms <= signed.manifest.issued_at_ms {
            return Err("manifest expiry must be after issued_at".to_string());
        }
        if now_ms >= signed.manifest.expires_at_ms {
            return Err(format!(
                "manifest expired at {}",
                signed.manifest.expires_at_ms
            ));
        }

        Ok(())
    }

    fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.signing_seed)
    }

    fn verifying_key(&self) -> VerifyingKey {
        self.signing_key().verifying_key()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUploadAuthorizationResponse {
    pub sha256: String,
    #[serde(default)]
    pub signed_get_manifest: Option<SignedArtifactTransferManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapArtifactFetchAuthorization {
    pub app_id: String,
    pub sha256: String,
    pub artifact_url: String,
    #[serde(default)]
    pub artifact_transfer_manifest: Option<SignedArtifactTransferManifest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority() -> ArtifactTransferAuthority {
        ArtifactTransferAuthority::derive("node-1", &[7u8; 32])
    }

    #[test]
    fn test_manifest_roundtrip_and_header_encoding() {
        let authority = authority();
        let signed = authority.issue_read_manifest("abc123");
        let header = signed.encode_header_value().unwrap();
        let decoded = SignedArtifactTransferManifest::decode_header_value(&header).unwrap();
        assert_eq!(decoded, signed);
    }

    #[test]
    fn test_manifest_verification_accepts_expected_values() {
        let authority = authority();
        let signed = authority.issue_write_manifest("deadbeef");
        authority
            .verify_manifest(
                &signed,
                "deadbeef",
                ArtifactTransferMethod::Put,
                signed.manifest.issued_at_ms,
            )
            .unwrap();
        assert!(signed.manifest.single_use);
    }

    #[test]
    fn test_manifest_verification_rejects_mismatched_digest() {
        let authority = authority();
        let signed = authority.issue_read_manifest("artifact-a");
        let err = authority
            .verify_manifest(
                &signed,
                "artifact-b",
                ArtifactTransferMethod::Get,
                signed.manifest.issued_at_ms,
            )
            .unwrap_err();
        assert!(err.contains("digest mismatch"));
    }

    #[test]
    fn test_manifest_verification_rejects_expired_manifest() {
        let authority = authority();
        let signed = authority.issue_manifest("abc123", ArtifactTransferMethod::Get, 1, false);
        let err = authority
            .verify_manifest(
                &signed,
                "abc123",
                ArtifactTransferMethod::Get,
                signed.manifest.expires_at_ms,
            )
            .unwrap_err();
        assert!(err.contains("expired"));
    }
}
