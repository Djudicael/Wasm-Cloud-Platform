use async_nats::jetstream;
use common::{artifact_transfer::ArtifactTransferAuthority, auth::AuthConfig};
use secrets::LocalSecretProvider;
use std::{path::PathBuf, sync::Arc};
use storage::Store;
use tokio::sync::RwLock;

pub const ARTIFACT_CREDENTIALS_APP_ID: &str = "_platform/artifact-credentials:v1";
pub const CLUSTER_NODE_STALE_AFTER_SECS: u64 = 120;
pub const ACTIVE_NODE_WAIT_TIMEOUT_SECS: u64 = 10;
pub const DEPLOY_INGRESS_LEASE_KEY: &str = "leader";

#[derive(Clone)]
pub struct SignaturePolicy {
    pub require_signature: bool,
    pub allowed_issuers: Vec<String>,
    pub allowed_identities: Vec<String>,
    pub allowed_repositories: Vec<String>,
    pub allowed_namespaces: Vec<String>,
}

#[derive(Clone)]
pub struct ArtifactReferencePolicy {
    pub require_oci_digest_refs: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub ingress_id: String,
    pub auth: Arc<RwLock<AuthConfig>>,
    pub store: Store,
    pub secret_provider: Arc<LocalSecretProvider>,
    pub bus: messaging::NatsBus,
    pub artifact_server_url: String,
    pub artifact_transfer_authority: ArtifactTransferAuthority,
    pub audit_path: PathBuf,
    pub credential_kv: jetstream::kv::Store,
    pub credential_kek_bytes: [u8; 32],
    pub ha_enabled: bool,
    pub leader_state: Arc<RwLock<LeaderState>>,
    pub signature_policy: SignaturePolicy,
    pub artifact_reference_policy: ArtifactReferencePolicy,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LeaderState {
    pub is_leader: bool,
    pub leader_ingress_id: Option<String>,
    pub leader_artifact_server_url: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LeaderLeaseRecord {
    pub ingress_id: String,
    pub artifact_server_url: String,
    pub updated_at_unix_secs: u64,
}
