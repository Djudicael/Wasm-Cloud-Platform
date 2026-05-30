mod app;
mod audit;
mod config;
mod crypto;
mod ha;
mod http;
mod signature;

use anyhow::Context;
use app::{AppState, ArtifactReferencePolicy, LeaderState, SignaturePolicy};
use async_nats::jetstream;
use clap::Parser;
use common::{artifact_transfer::ArtifactTransferAuthority, auth::AuthConfig};
use config::{parse_csv_list, socket_addr, Args};
use crypto::load_kek;
use ha::{
    get_or_create_kv, start_leader_lease_task, subscribe_artifact_replication,
    subscribe_cluster_updates,
};
use messaging::NatsBus;
use secrets::{crypto::SymmetricKey, LocalSecretProvider};
use std::sync::Arc;
use storage::{artifact_server::ArtifactPeerTokenConfig, Store};
use tokio::sync::RwLock;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "deploy_ingress=info,messaging=info,storage=info".to_string()),
        )
        .with_target(true)
        .init();

    let args = Args::parse();
    let store = Store::open(&args.db_path)?;
    let kek = load_kek(&args)?;
    let credential_kek_bytes = *kek.as_bytes();
    let artifact_transfer_authority =
        ArtifactTransferAuthority::derive(&args.ingress_id, kek.as_bytes());
    let secret_provider = Arc::new(LocalSecretProvider::new(
        store.clone(),
        SymmetricKey::from_bytes(credential_kek_bytes),
    ));

    let mut bus = if let Some(creds) = args.nats_creds.as_deref() {
        NatsBus::connect_secure(&args.nats_url, creds).await?
    } else {
        NatsBus::connect(&args.nats_url).await?
    };
    bus.set_node_id(args.ingress_id.clone());
    bus.setup_jetstream().await?;
    let js = jetstream::new(bus.client().clone());
    let leader_kv = get_or_create_kv(
        &js,
        &args.ha_lease_bucket,
        Some(std::time::Duration::from_secs(args.ha_lease_ttl_secs)),
    )
    .await?;
    let credential_kv = get_or_create_kv(&js, &args.credential_bucket, None).await?;

    let artifact_server_url = args
        .advertised_artifact_url
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", args.artifact_port));
    let auth = Arc::new(RwLock::new(AuthConfig {
        enabled: args.auth_enabled,
        read_token: args.auth_read_token.clone(),
        write_token: args.auth_write_token.clone(),
        require_tls: false,
        ..Default::default()
    }));
    auth.read().await.validate().map_err(anyhow::Error::msg)?;

    subscribe_cluster_updates(&bus, store.clone()).await?;
    let leader_state = Arc::new(RwLock::new(LeaderState {
        is_leader: !args.ha_enabled,
        leader_ingress_id: if args.ha_enabled {
            None
        } else {
            Some(args.ingress_id.clone())
        },
        leader_artifact_server_url: if args.ha_enabled {
            None
        } else {
            Some(artifact_server_url.clone())
        },
    }));

    let state = AppState {
        ingress_id: args.ingress_id.clone(),
        auth,
        store: store.clone(),
        secret_provider,
        bus,
        artifact_server_url: artifact_server_url.clone(),
        artifact_transfer_authority: artifact_transfer_authority.clone(),
        audit_path: args.audit_path.clone(),
        credential_kv,
        credential_kek_bytes,
        ha_enabled: args.ha_enabled,
        leader_state,
        signature_policy: SignaturePolicy {
            require_signature: args.require_signature,
            allowed_issuers: parse_csv_list(args.allowed_issuers.as_deref()),
            allowed_identities: parse_csv_list(args.allowed_identities.as_deref()),
            allowed_repositories: parse_csv_list(args.allowed_repositories.as_deref()),
            allowed_namespaces: parse_csv_list(args.allowed_namespaces.as_deref()),
        },
        artifact_reference_policy: ArtifactReferencePolicy {
            require_oci_digest_refs: args.require_oci_digest_refs,
        },
    };

    if args.ha_enabled {
        start_leader_lease_task(
            state.clone(),
            leader_kv,
            std::time::Duration::from_secs(args.ha_lease_refresh_secs),
        );
        subscribe_artifact_replication(&state).await?;
    }

    let deploy_app = http::router(state.clone());
    let artifact_app = storage::artifact_server::artifact_router(
        store,
        Vec::<ArtifactPeerTokenConfig>::new(),
        Some(artifact_transfer_authority),
    );

    let deploy_addr = socket_addr(&args.bind_address, args.deploy_port)?;
    let artifact_addr = socket_addr(&args.bind_address, args.artifact_port)?;

    info!(
        ingress_id = %args.ingress_id,
        deploy_addr = %deploy_addr,
        artifact_addr = %artifact_addr,
        artifact_server_url = %artifact_server_url,
        "deploy ingress starting"
    );

    let deploy_listener = tokio::net::TcpListener::bind(deploy_addr).await?;
    let artifact_listener = tokio::net::TcpListener::bind(artifact_addr).await?;

    tokio::try_join!(
        async move {
            axum::serve(
                deploy_listener,
                deploy_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .context("deploy ingress server failed")
        },
        async move {
            axum::serve(
                artifact_listener,
                artifact_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .context("deploy ingress artifact server failed")
        }
    )?;

    Ok(())
}
