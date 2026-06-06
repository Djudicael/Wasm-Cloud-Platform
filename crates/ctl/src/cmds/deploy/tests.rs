use super::args::DeployArgs;
use super::artifact::{
    load_cluster_node_registry, remote_source_from_oci_reference, request_per_node_manifests,
    resolve_artifact_input, select_target_node_ids, ArtifactInput,
};
use super::payload::{build_deploy_payload, build_gateway_config};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use common::{
    artifact_transfer::{
        ArtifactManifestBatchRequest, ArtifactManifestBatchResponse, ArtifactTransferAuthority,
    },
    health::NodeHealthStatus,
    types::ClusterNodeRecord,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
struct TestState {
    nodes: Vec<ClusterNodeRecord>,
    requested_audiences: Arc<Mutex<Vec<String>>>,
}

fn active_node(node_id: &str, last_seen_unix_secs: u64) -> ClusterNodeRecord {
    ClusterNodeRecord {
        node_id: node_id.to_string(),
        last_seen_unix_secs,
        joined_at_unix_secs: Some(last_seen_unix_secs),
        health_status: NodeHealthStatus::Healthy,
        proxy_address: Some(format!("{node_id}.internal:8080")),
        artifact_server_url: Some(format!("http://{node_id}.internal:9091")),
        protocol_version: Some(common::protocol::PROTOCOL_VERSION),
        binary_version: Some(common::protocol::BINARY_VERSION.to_string()),
        secret_transport_public_key: None,
        accepting_requests: Some(true),
        active_instances: Some(1),
        deployed_apps: Some(1),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Parser)]
struct DeployCliTestHarness {
    #[command(flatten)]
    args: DeployArgs,
}

#[test]
fn test_deploy_gateway_rate_limit_defaults_to_node_local() {
    let parsed = DeployCliTestHarness::parse_from([
        "wasm-ctl",
        "--app",
        "api",
        "--wasm",
        "./test.wasm",
        "--gateway-rps",
        "100",
        "--gateway-rps-burst",
        "20",
    ]);

    let gateway = build_gateway_config(&parsed.args).unwrap();
    assert!(!gateway.rate_limit.unwrap().distributed);
}

#[test]
fn test_deploy_gateway_rate_limit_supports_explicit_distributed_opt_in() {
    let parsed = DeployCliTestHarness::parse_from([
        "wasm-ctl",
        "--app",
        "api",
        "--wasm",
        "./test.wasm",
        "--gateway-rps",
        "100",
        "--gateway-rps-burst",
        "20",
        "--gateway-rps-distributed",
    ]);

    let gateway = build_gateway_config(&parsed.args).unwrap();
    assert!(gateway.rate_limit.unwrap().distributed);
}

#[test]
fn test_remote_source_from_oci_reference_preserves_reference() {
    let source = remote_source_from_oci_reference(
        "oci://ghcr.io/example-org/example-app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        Some("ghcr-reader".to_string()),
        None,
    )
    .unwrap();
    assert_eq!(
        source.reference.as_deref(),
        Some(
            "oci://ghcr.io/example-org/example-app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
    );
    assert!(source.url.is_empty());
    assert!(source.sha256.is_empty());
    assert_eq!(source.credential_ref.as_deref(), Some("ghcr-reader"));
}

#[test]
fn test_resolve_artifact_input_accepts_cli_oci_reference() {
    let parsed = DeployCliTestHarness::parse_from([
        "wasm-ctl",
        "--app",
        "api",
        "--artifact-ref",
        "oci://ghcr.io/example-org/example-app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--artifact-credential",
        "ghcr-reader",
    ]);

    let resolved = resolve_artifact_input(&parsed.args, None).unwrap();
    match resolved {
        ArtifactInput::Remote(source) => {
            assert_eq!(
                source.reference.as_deref(),
                Some(
                    "oci://ghcr.io/example-org/example-app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                )
            );
            assert!(source.url.is_empty());
            assert!(source.sha256.is_empty());
            assert_eq!(source.credential_ref.as_deref(), Some("ghcr-reader"));
        }
        ArtifactInput::LocalPath(_) => panic!("expected remote artifact input"),
    }
}

#[test]
fn test_build_artifact_signature_accepts_cosign_payload_mode() {
    let parsed = DeployCliTestHarness::parse_from([
        "wasm-ctl",
        "--app",
        "api",
        "--artifact-ref",
        "oci://ghcr.io/example-org/example-app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--artifact-public-key",
        "cHVibGljLWtleQ==",
        "--artifact-signature",
        "c2lnbmF0dXJl",
        "--artifact-signature-algorithm",
        "cosign-ed25519",
        "--artifact-signature-payload",
        "{\"critical\":{\"identity\":{\"docker-reference\":\"ghcr.io/example-org/example-app\"},\"image\":{\"docker-manifest-digest\":\"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"},\"type\":\"cosign container image signature\"}}",
    ]);

    let resolved = resolve_artifact_input(&parsed.args, None).unwrap();
    match resolved {
        ArtifactInput::Remote(source) => {
            let signature = source.signature.expect("signature missing");
            assert_eq!(signature.algorithm, "cosign-ed25519");
            assert!(signature.payload.is_some());
        }
        ArtifactInput::LocalPath(_) => panic!("expected remote artifact input"),
    }
}

#[test]
fn test_build_artifact_signature_accepts_sigstore_bundle_mode() {
    let parsed = DeployCliTestHarness::parse_from([
        "wasm-ctl",
        "--app",
        "api",
        "--artifact-url",
        "https://artifacts.example.com/api.wasm",
        "--sha256",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--artifact-public-key",
        "cHVibGljLWtleQ==",
        "--artifact-signature",
        "c2lnbmF0dXJl",
        "--artifact-signature-algorithm",
        "sigstore-bundle",
        "--artifact-signature-payload",
        "{\"mediaType\":\"application/vnd.dev.sigstore.bundle+json;version=0.3\"}",
        "--artifact-identity",
        "user@example.com",
        "--artifact-issuer",
        "https://github.com/login/oauth",
    ]);

    let resolved = resolve_artifact_input(&parsed.args, None).unwrap();
    match resolved {
        ArtifactInput::Remote(source) => {
            let signature = source.signature.expect("signature missing");
            assert_eq!(signature.algorithm, "sigstore-bundle");
            assert!(signature.payload.is_some());
            assert_eq!(signature.identity.as_deref(), Some("user@example.com"));
        }
        ArtifactInput::LocalPath(_) => panic!("expected remote artifact input"),
    }
}

#[test]
fn test_build_deploy_payload_extracts_manifest_routes() {
    let parsed = DeployCliTestHarness::parse_from([
        "wasm-ctl",
        "--manifest",
        "app.toml",
        "--artifact-url",
        "https://artifacts.example.com/api.wasm",
        "--sha256",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ]);
    let manifest = super::super::manifest::DeployManifest {
        app: super::super::manifest::AppManifestSection {
            name: "api".to_string(),
            version: "v1".to_string(),
            namespace: "tenant-a".to_string(),
            description: String::new(),
            wasm_artifact: String::new(),
            wasm_bind_port: 8080,
        },
        fuel: super::super::manifest::FuelManifestSection::default(),
        policy: None,
        gateway: Some(super::super::manifest::GatewayManifestSection {
            host: Some("api.example.com".to_string()),
            routes: vec![super::super::manifest::RouteManifestSection {
                host: "api.example.com".to_string(),
                path_prefix: "/v1".to_string(),
                strip_prefix: true,
            }],
            auth: None,
            cors: None,
            rate_limit: None,
            circuit_breaker: None,
            transform: None,
            endpoints: Vec::new(),
        }),
        env: HashMap::new(),
        secrets: HashMap::new(),
        api_keys: Vec::new(),
        artifact: None,
    };
    let app_id = common::types::AppId::new_namespaced("tenant-a", "api", "v1");

    let (_config, _gateway, routes, _api_keys) =
        build_deploy_payload(&parsed.args, Some(&manifest), &app_id, "tenant-a").unwrap();

    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].host, "api.example.com");
    assert_eq!(routes[0].path_prefix, "/");
    assert_eq!(routes[1].path_prefix, "/v1");
    assert!(routes[1].strip_prefix);
}

#[tokio::test]
async fn test_registry_backed_manifest_fanout_uses_exact_active_peers() {
    let authority = ArtifactTransferAuthority::derive("node-1", &[9u8; 32]);
    let requested_audiences = Arc::new(Mutex::new(Vec::new()));
    let state = TestState {
        nodes: vec![
            active_node("node-1", now_secs()),
            active_node("node-2", now_secs()),
            active_node("node-3", now_secs().saturating_sub(10_000)),
        ],
        requested_audiences: requested_audiences.clone(),
    };

    async fn cluster_nodes(State(state): State<TestState>) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "nodes": state.nodes,
            "active_staleness_secs": 60,
        }))
    }

    async fn authorize(
        State(state): State<TestState>,
        Json(body): Json<ArtifactManifestBatchRequest>,
    ) -> Json<ArtifactManifestBatchResponse> {
        *state.requested_audiences.lock().await = body.audiences.clone();
        let authority = ArtifactTransferAuthority::derive("node-1", &[9u8; 32]);
        Json(ArtifactManifestBatchResponse {
            sha256: "abc123".to_string(),
            manifests: body
                .audiences
                .into_iter()
                .map(|audience_node_id| {
                    common::artifact_transfer::ArtifactManifestAudienceBinding {
                        artifact_transfer_manifest: authority
                            .issue_read_manifest_for_audience("abc123", &audience_node_id),
                        audience_node_id,
                    }
                })
                .collect(),
        })
    }

    let app = Router::new()
        .route("/admin/cluster/nodes", get(cluster_nodes))
        .route("/artifacts/abc123/authorize", post(authorize))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let http = reqwest::Client::new();
    let base_url = format!("http://{}", addr);
    let registry = load_cluster_node_registry(&http, &base_url).await.unwrap();
    assert_eq!(registry.active_staleness_secs, 60);
    let target_node_ids = select_target_node_ids(
        registry.nodes,
        Some("node-1"),
        registry.active_staleness_secs,
    );
    assert_eq!(target_node_ids, vec!["node-2".to_string()]);

    let manifests = request_per_node_manifests(&http, &base_url, "abc123", &target_node_ids)
        .await
        .unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].audience_node_id, "node-2");
    assert_eq!(
        *requested_audiences.lock().await,
        vec!["node-2".to_string()]
    );
    assert_eq!(
        manifests[0].artifact_transfer_manifest.manifest.issuer,
        authority.local_node_id()
    );
}
