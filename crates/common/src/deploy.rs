use crate::artifact_transfer::ArtifactManifestAudienceBinding;
use crate::types::{ApiKeyRecord, AppConfig, AppId, GatewayRouteConfig, Route};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSignature {
    #[serde(default = "default_signature_algorithm")]
    pub algorithm: String,
    pub public_key: String,
    pub signature: String,
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
}

fn default_signature_algorithm() -> String {
    "ed25519".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteArtifactSource {
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub credential_ref: Option<String>,
    #[serde(default)]
    pub signature: Option<ArtifactSignature>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteArtifactIngressRequest {
    pub artifact: RemoteArtifactSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteArtifactIngressResponse {
    pub source_node_id: String,
    pub artifact_url: String,
    pub expected_hash: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub artifact_transfer_manifests: Vec<ArtifactManifestAudienceBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployIntentRequest {
    pub app_id: AppId,
    pub config: AppConfig,
    #[serde(default)]
    pub gateway_config: Option<GatewayRouteConfig>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub api_keys: Vec<ApiKeyRecord>,
    pub artifact: RemoteArtifactSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployIntentResponse {
    pub app_id: AppId,
    pub artifact_url: String,
    pub expected_hash: String,
    pub size_bytes: u64,
    pub source_node_id: String,
    #[serde(default)]
    pub artifact_transfer_manifests: Vec<ArtifactManifestAudienceBinding>,
    #[serde(default)]
    pub gateway_config_published: bool,
    #[serde(default)]
    pub route_count: usize,
    #[serde(default)]
    pub api_key_count: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCredentialSetRequest {
    pub key: String,
    pub value: String,
}

impl std::fmt::Debug for ArtifactCredentialSetRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactCredentialSetRequest")
            .field("key", &self.key)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactCredentialSetResponse {
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactVerificationRecord {
    pub sha256: String,
    pub verified: bool,
    #[serde(default)]
    pub algorithm: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub public_key_sha256: Option<String>,
    pub verified_at_unix_secs: u64,
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn artifact_credential_debug_redacts_value() {
        let sentinel = "p10-secret-sentinel-123456";
        let request = ArtifactCredentialSetRequest {
            key: "registry".to_string(),
            value: sentinel.to_string(),
        };
        let output = format!("{request:?}");
        assert!(!output.contains(sentinel));
        assert!(output.contains("[REDACTED]"));
    }
}
