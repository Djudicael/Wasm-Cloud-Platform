//! End-to-end test: Deploy a Wasm app inside a microVM.
//!
//! This test:
//! 1. Starts a NATS container on the host runtime
//! 2. Spawns a wasm-node microVM
//! 3. Uploads hello-axum.wasm to the artifact server
//! 4. Deploys the app via NATS
//! 5. Sends an HTTP request through the VM's proxy
//! 6. Verifies the response
//!
//! ## Prerequisites
//!
//! - Firecracker installed and in PATH
//! - `/dev/kvm` accessible (user in `kvm` group)
//! - VM images built (`./scripts/vm/build-all-images.sh`)
//! - `sudo` or `CAP_NET_ADMIN` for TAP creation
//!
//! ## Run
//!
//! ```bash
//! sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture
//! ```

use common::container_runtime::NatsContainer;
use std::time::Duration;
use vm_testbed::{MicroVm, VmConfig};

/// Test that a wasm-node microVM can deploy and serve a Wasm application.
///
/// This is the most basic VM test — it validates that the entire stack
/// (kernel, init, wasm-node binary, NATS connectivity, Wasm runtime)
/// works correctly inside a microVM.
#[tokio::test]
#[cfg_attr(not(feature = "firecracker"), ignore = "requires firecracker feature")]
async fn test_single_node_deploy() {
    tracing_subscriber::fmt::init();

    // 1. Start NATS container on host (re-use existing E2E infrastructure)
    let nats_container = start_nats_container()
        .await
        .expect("Failed to start NATS container");
    let nats_url = nats_container.url.clone();

    // 2. Spawn wasm-node microVM
    let vm_config = VmConfig {
        id: "test-node-0".to_string(),
        kernel_path: find_kernel(),
        rootfs_path: find_node_rootfs(),
        data_drive_path: None,
        memory_mb: 512,
        vcpus: 2,
        ip: "172.20.0.2".to_string(),
        gateway: "172.20.0.1".to_string(),
        bridge_name: "br-wasm".to_string(),
        tap_device: "tap-test-node-0".to_string(),
        extra_kernel_args: Vec::new(),
        mmds_data: Some(serde_json::json!({
            "node_config": {
                "node_id": "test-node-0",
                "nats_url": nats_url,
                "proxy_port": 8080,
                "admin_port": 9090,
                "artifact_port": 9091,
            }
        })),
    };

    let mut vm = MicroVm::spawn(vm_config)
        .await
        .expect("Failed to spawn microVM");

    // 3. Wait for node to be healthy
    vm.wait_for_health(Duration::from_secs(60))
        .await
        .expect("Node did not become healthy");

    println!("✅ MicroVM is healthy at {}", vm.admin_addr());

    // 4. Build and upload the test Wasm app
    let wasm_path = build_test_wasm().expect("Failed to build test Wasm app");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");

    // Upload via artifact server
    let artifact_url = upload_artifact(&vm, &wasm_path)
        .await
        .expect("Failed to upload artifact");

    // 5. Deploy the app via NATS
    deploy_app(&nats_url, "hello-axum:v1", &artifact_url, &sha256)
        .await
        .expect("Failed to deploy app");

    // 6. Add route
    add_route(&nats_url, "test-app.local", "hello-axum:v1")
        .await
        .expect("Failed to add route");

    // 7. Wait for app to be ready
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 8. Send HTTP request through the VM proxy
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let url = format!("http://{}/", vm.proxy_addr());
    let resp = client
        .get(&url)
        .header("Host", "test-app.local")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(resp.status(), 200, "Expected 200 OK, got {}", resp.status());

    let body = resp.text().await.expect("Failed to read body");
    println!("✅ Response body: {}", body);

    // 9. Cleanup
    vm.shutdown().await.expect("Failed to shutdown VM");
    println!("✅ Test passed!");
}

// ── Helpers ──────────────────────────────────────────────────────────

async fn start_nats_container() -> Result<NatsContainer, Box<dyn std::error::Error>> {
    let container = NatsContainer::start(4222)?;
    Ok(container)
}

fn find_kernel() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("VM_KERNEL_PATH") {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    let candidates = ["./assets/vmlinux-6.1", "/opt/vm-testbed/vmlinux-6.1"];
    for c in &candidates {
        let p = std::path::PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }
    panic!("Kernel not found. Set VM_KERNEL_PATH or run ./scripts/vm/build-kernel.sh")
}

fn find_node_rootfs() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("VM_NODE_ROOTFS") {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    let candidates = [
        "./assets/wasm-node-rootfs.ext4",
        "/opt/vm-testbed/wasm-node-rootfs.ext4",
    ];
    for c in &candidates {
        let p = std::path::PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }
    panic!("Rootfs not found. Set VM_NODE_ROOTFS or run ./scripts/vm/build-node-rootfs.sh")
}

fn build_test_wasm() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    // Check if pre-built wasm exists
    let candidates = [
        "./apps/hello-axum/target/wasm32-wasip2/release/hello-axum.wasm",
        "./target/wasm32-wasip2/release/hello-axum.wasm",
    ];
    for c in &candidates {
        let p = std::path::PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }

    // Build it
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            "apps/hello-axum/Cargo.toml",
            "--target",
            "wasm32-wasip2",
            "--release",
        ])
        .env("RUSTFLAGS", "--cfg tokio_unstable")
        .status()?;

    if !status.success() {
        return Err("Failed to build hello-axum wasm".into());
    }

    Ok(std::path::PathBuf::from(
        "./apps/hello-axum/target/wasm32-wasip2/release/hello-axum.wasm",
    ))
}

fn compute_sha256(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path)?;
    let hash = Sha256::digest(&data);
    Ok(hex::encode(hash))
}

async fn upload_artifact(
    vm: &MicroVm,
    wasm_path: &std::path::Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://{}/artifacts", vm.admin_addr());

    let filename = wasm_path.file_name().unwrap().to_str().unwrap();
    let data = tokio::fs::read(wasm_path).await?;

    let resp = client
        .post(&url)
        .header("X-Artifact-Name", filename)
        .body(data)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!("Artifact upload failed: {}", resp.status()).into());
    }

    Ok(format!("http://{}/artifacts/{}", vm.admin_addr(), filename))
}

async fn deploy_app(
    nats_url: &str,
    app_id: &str,
    artifact_url: &str,
    sha256: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = async_nats::connect(nats_url).await?;

    let deploy_event = serde_json::json!({
        "app_id": app_id,
        "artifact_url": artifact_url,
        "sha256": sha256,
        "config": {
            "fuel_limit": 100_000_000,
            "memory_limit_mb": 128,
            "min_instances": 1,
            "max_instances": 3,
        }
    });

    client
        .publish("deploy.apps", deploy_event.to_string().into())
        .await?;

    Ok(())
}

async fn add_route(
    nats_url: &str,
    host: &str,
    app_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = async_nats::connect(nats_url).await?;

    let route_event = serde_json::json!({
        "host": host,
        "app_id": app_id,
    });

    client
        .publish("routes.add", route_event.to_string().into())
        .await?;

    Ok(())
}
