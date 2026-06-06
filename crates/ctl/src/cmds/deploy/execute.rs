use super::args::DeployArgs;
use super::artifact::{
    load_cluster_node_registry, request_per_node_manifests, resolve_artifact_input,
    select_target_node_ids, submit_deploy_intent, ArtifactInput,
};
use super::payload::build_deploy_payload;
use anyhow::Result;
use colored::Colorize;
use common::artifact_transfer::ArtifactUploadAuthorizationResponse;
use common::deploy::DeployIntentRequest;
use common::types::AppId;
use messaging::{events::Event, NatsBus};
use sha2::{Digest, Sha256};

/// Deploy through either the remote deploy-intent path or the local artifact
/// upload path while keeping the CLI surface stable.
pub async fn run(
    args: DeployArgs,
    bus: &NatsBus,
    default_node_api: &str,
    default_deploy_api: Option<&str>,
    http: &reqwest::Client,
) -> Result<()> {
    let manifest = if let Some(ref path) = args.manifest {
        Some(super::super::manifest::DeployManifest::from_toml(path)?)
    } else {
        None
    };

    let app_name = args
        .app
        .clone()
        .or_else(|| manifest.as_ref().map(|m| m.app.name.clone()))
        .ok_or_else(|| anyhow::anyhow!("--app is required when no manifest is provided"))?;
    let version = args
        .version
        .clone()
        .or_else(|| manifest.as_ref().map(|m| m.app.version.clone()))
        .unwrap_or_else(|| "v1".to_string());
    let namespace = if args.namespace != "default" {
        args.namespace.clone()
    } else {
        manifest
            .as_ref()
            .map(|m| m.app.namespace.clone())
            .unwrap_or_else(|| args.namespace.clone())
    };
    let artifact_input = resolve_artifact_input(&args, manifest.as_ref())?;
    let wasm_path = match &artifact_input {
        ArtifactInput::LocalPath(path) => Some(path.clone()),
        ArtifactInput::Remote(_) => None,
    };

    let app_id = AppId::new_namespaced(&namespace, &app_name, &version);
    let node_api = args.node_api.as_deref().unwrap_or(default_node_api);
    let deploy_api = args
        .deploy_api
        .as_deref()
        .or(default_deploy_api)
        .unwrap_or(node_api);

    if let ArtifactInput::Remote(artifact) = artifact_input {
        let source_display = artifact
            .reference
            .as_deref()
            .unwrap_or(artifact.url.as_str())
            .to_string();
        let displayed_hash = if artifact.sha256.trim().is_empty() {
            "resolved by registry".to_string()
        } else {
            artifact.sha256.clone()
        };
        let (config, gateway_config, routes, api_keys) =
            build_deploy_payload(&args, manifest.as_ref(), &app_id, &namespace)?;

        println!("{}", "Deploying application:".bold());
        println!("  App ID:  {}", app_id.0.cyan());
        println!("  Namespace: {}", namespace.green());
        println!("  SHA-256: {}", displayed_hash.yellow());
        println!("  Source:  {}", source_display.cyan());
        if let Some(credential_ref) = artifact.credential_ref.as_deref() {
            println!("  Credential Ref: {}", credential_ref.green());
        }
        println!("\n{}", "Submitting deploy intent...".bold());

        let response = submit_deploy_intent(
            http,
            deploy_api,
            DeployIntentRequest {
                app_id: app_id.clone(),
                config,
                gateway_config,
                routes,
                api_keys,
                artifact: *artifact,
            },
        )
        .await?;

        println!(
            "{} Deploy intent accepted for {}",
            "OK".green(),
            response.app_id.0.cyan()
        );
        println!("  Artifact URL: {}", response.artifact_url.cyan());
        println!("  Resolved SHA-256: {}", response.expected_hash.yellow());
        println!("  Size: {} bytes", response.size_bytes);
        if response.gateway_config_published {
            println!(
                "{} Gateway config published for {}",
                "OK".green(),
                response.app_id.0.cyan()
            );
        }
        if response.route_count > 0 {
            println!(
                "{} Route bindings published for {} ({})",
                "OK".green(),
                response.app_id.0.cyan(),
                response.route_count
            );
        }
        if response.api_key_count > 0 {
            println!(
                "{} API keys stored for {} ({})",
                "OK".green(),
                response.app_id.0.cyan(),
                response.api_key_count
            );
        }
        println!(
            "{} Deploy event published for {} - all nodes are compiling.",
            "OK".green(),
            response.app_id.0.cyan()
        );
        return Ok(());
    }

    let wasm_path = wasm_path.expect("local deploy path must be present");
    let wasm_bytes = std::fs::read(&wasm_path)
        .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", wasm_path, e))?;

    let size_bytes = wasm_bytes.len() as u64;
    let sha256 = hex::encode(Sha256::digest(&wasm_bytes));
    println!("{}", "Deploying application:".bold());
    println!("  App ID:  {}", app_id.0.cyan());
    println!("  Namespace: {}", namespace.green());
    println!("  SHA-256: {}", sha256.yellow());
    println!(
        "  Size:    {} bytes ({:.1} MB)",
        size_bytes,
        size_bytes as f64 / 1_048_576.0
    );

    let upload_url = format!("{}/artifacts/{}", node_api, sha256);
    let artifact_url = upload_url.clone();

    println!("\n{}", "Uploading artifact...".bold());
    let pb = indicatif::ProgressBar::new(size_bytes);
    pb.set_style(
        indicatif::ProgressStyle::default_bar()
            .template(
                "[{elapsed_precise}] {bar:40.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec})",
            )
            .unwrap()
            .progress_chars("=>-"),
    );

    let resp = http.put(&upload_url).body(wasm_bytes).send().await?;
    pb.finish_with_message("uploaded");

    if !resp.status().is_success() {
        anyhow::bail!("Artifact upload failed: {}", resp.status());
    }
    let upload_authorization = resp
        .json::<ArtifactUploadAuthorizationResponse>()
        .await
        .ok();
    println!("{} Artifact uploaded to {}", "OK".green(), upload_url);

    let upload_source_node_id = upload_authorization
        .as_ref()
        .and_then(|authorization| authorization.signed_get_manifest.as_ref())
        .map(|manifest| manifest.manifest.issuer.clone());

    let registered_nodes = load_cluster_node_registry(http, node_api).await?;
    let target_node_ids = select_target_node_ids(
        registered_nodes.nodes,
        upload_source_node_id.as_deref(),
        registered_nodes.active_staleness_secs,
    );

    let per_node_manifests =
        match request_per_node_manifests(http, node_api, &sha256, &target_node_ids).await {
            Ok(manifests) => manifests,
            Err(e) => {
                eprintln!(
                    "warning: failed to request per-node artifact manifests: {}",
                    e
                );
                Vec::new()
            }
        };

    let (config, gateway_config, routes, api_keys) =
        build_deploy_payload(&args, manifest.as_ref(), &app_id, &namespace)?;

    if target_node_ids.is_empty() {
        eprintln!(
            "warning: the authoritative cluster node registry contains no active peer nodes beyond the upload source; no per-node artifact manifests were needed"
        );
    }

    let event = Event::DeployApp {
        app_id: app_id.clone(),
        config,
        artifact_url,
        artifact_transfer_manifests: per_node_manifests,
        expected_hash: Some(sha256),
        size_bytes,
    };
    bus.publish(&event).await?;

    if let Some(gateway_config) = gateway_config {
        let gw_event = Event::GatewayConfigUpdate {
            app_id: app_id.clone(),
            config: gateway_config,
        };
        bus.publish(&gw_event).await?;
        println!(
            "{} Gateway config published for {}",
            "OK".green(),
            app_id.0.cyan()
        );
    }

    let route_count = routes.len();
    for route in routes {
        let route_event = Event::RouteAdd { route };
        bus.publish(&route_event).await?;
    }
    if route_count > 0 {
        println!(
            "{} Route bindings published for {} ({})",
            "OK".green(),
            app_id.0.cyan(),
            route_count
        );
    }

    if !api_keys.is_empty() {
        let url = format!("{}/admin/api_keys/{}", node_api, app_id.0);
        let resp = http.post(&url).json(&api_keys).send().await?;
        if resp.status().is_success() {
            println!("{} API keys stored for {}", "OK".green(), app_id.0.cyan());
        } else {
            println!("warning: failed to store API keys: {}", resp.status());
        }
    }

    println!(
        "{} Deploy event published for {} - all nodes are compiling.",
        "OK".green(),
        app_id.0.cyan()
    );
    Ok(())
}

pub async fn remove(app_id_str: &str, bus: &NatsBus) -> Result<()> {
    let (name, version) = app_id_str
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("app_id must be <name>:<version>"))?;
    let event = Event::RemoveApp {
        app_id: AppId::new(name, version),
    };
    bus.publish(&event).await?;
    println!(
        "{} Remove event published for {}",
        "✓".green(),
        app_id_str.cyan()
    );
    Ok(())
}
