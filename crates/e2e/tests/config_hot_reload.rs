//! E2E tests for configuration hot-reload via the admin API.
//!
//! These tests verify that:
//! - `GET /admin/config` returns cold + hot config
//! - `PATCH /admin/config` applies partial updates to hot-reloadable fields
//! - `DELETE /admin/config` resets hot config to startup defaults
//! - Rate-limit changes take effect without restart
//! - Log-level changes take effect without restart
//! - Hot-config overrides survive a node restart (persisted in redb)
//!
//! # Prerequisites
//!
//! - A running NATS server on port 4222 (or set `NATS_URL`)
//! - The `wasm-node` binary built in debug mode
//!
//! # Running
//!
//! ```sh
//! cargo test -p e2e --test config_hot_reload -- --ignored --test-threads=1
//! ```

mod harness;

use harness::{NatsContainer, NodeProcess};
use reqwest::StatusCode;
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;

/// Helper: build the admin URL for a node.
fn admin_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{}{}", port, path)
}

/// Helper: GET the current config from the admin API.
async fn get_config(http: &reqwest::Client, admin_port: u16) -> serde_json::Value {
    let url = admin_url(admin_port, "/admin/config");
    let resp = http
        .get(&url)
        .send()
        .await
        .expect("GET /admin/config failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /admin/config should return 200"
    );
    resp.json()
        .await
        .expect("response body should be valid JSON")
}

/// Helper: PATCH the hot config with a partial update.
async fn patch_config(
    http: &reqwest::Client,
    admin_port: u16,
    body: &serde_json::Value,
) -> reqwest::Response {
    let url = admin_url(admin_port, "/admin/config");
    http.patch(&url)
        .json(body)
        .send()
        .await
        .expect("PATCH /admin/config failed")
}

/// Helper: DELETE (reset) the hot config.
async fn delete_config(http: &reqwest::Client, admin_port: u16) -> reqwest::Response {
    let url = admin_url(admin_port, "/admin/config");
    http.delete(&url)
        .send()
        .await
        .expect("DELETE /admin/config failed")
}

// ─── Test: GET /admin/config returns expected structure ────────────────────

#[tokio::test]
async fn test_get_config_returns_cold_and_hot() {
    let nats = NatsContainer::start(14229)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-get", &nats.url, 18080, 19091, 19090)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();

    // Give the node a moment to fully initialise the admin API
    sleep(Duration::from_secs(2)).await;

    let config = get_config(&http, node.admin_port).await;

    // Cold config should contain key startup fields
    let cold = &config["cold"];
    assert!(
        cold["node_id"].is_string(),
        "cold.node_id should be a string"
    );
    assert!(
        cold["proxy_http_port"].is_number(),
        "cold.proxy_http_port should be a number"
    );
    assert!(
        cold["nats_url"].is_string(),
        "cold.nats_url should be a string"
    );

    // Hot config should contain all hot-reloadable sections
    let hot = &config["hot"];
    assert!(
        hot["rate_limit"].is_object(),
        "hot.rate_limit should be an object"
    );
    assert!(hot["ebpf"].is_object(), "hot.ebpf should be an object");
    assert!(hot["gc"].is_object(), "hot.gc should be an object");
    assert!(hot["health"].is_object(), "hot.health should be an object");
    assert!(
        hot["logging"].is_object(),
        "hot.logging should be an object"
    );

    // The hot_reloadable_fields list should be present
    assert!(
        config["hot_reloadable_fields"].is_array(),
        "hot_reloadable_fields should be an array"
    );
}

// ─── Test: PATCH /admin/config applies partial updates ─────────────────────

#[tokio::test]
async fn test_patch_config_updates_hot_fields() {
    let nats = NatsContainer::start(14230)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-patch", &nats.url, 18081, 19092, 19091)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Read the initial rate-limit RPS
    let before = get_config(&http, node.admin_port).await;
    let initial_rps = before["hot"]["rate_limit"]["default_requests_per_second"]
        .as_u64()
        .expect("initial RPS should be a number");

    // Apply a partial update: change only rate_limit_default_rps
    let new_rps = 9999u64;
    let body = json!({
        "rate_limit_default_rps": new_rps,
    });

    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PATCH should return 200, got {}",
        resp.status()
    );

    let result: serde_json::Value = resp.json().await.expect("response should be JSON");
    assert_eq!(result["status"], "updated");
    assert_eq!(result["changes_applied"], 1);

    // Verify the change took effect
    let after = get_config(&http, node.admin_port).await;
    let actual_rps = after["hot"]["rate_limit"]["default_requests_per_second"]
        .as_u64()
        .expect("updated RPS should be a number");
    assert_eq!(
        actual_rps, new_rps,
        "RPS should have been updated from {} to {}",
        initial_rps, new_rps
    );

    // Other fields should be unchanged
    let burst = after["hot"]["rate_limit"]["default_burst_capacity"]
        .as_u64()
        .expect("burst should be a number");
    let initial_burst = before["hot"]["rate_limit"]["default_burst_capacity"]
        .as_u64()
        .expect("initial burst should be a number");
    assert_eq!(burst, initial_burst, "burst should be unchanged");
}

// ─── Test: PATCH with multiple fields ──────────────────────────────────────

#[tokio::test]
async fn test_patch_config_multiple_fields() {
    let nats = NatsContainer::start(14231)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-multi", &nats.url, 18082, 19093, 19092)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    let body = json!({
        "rate_limit_default_rps": 5000,
        "gc_interval_secs": 120,
        "logging_level": "debug",
    });

    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let result: serde_json::Value = resp.json().await.expect("response should be JSON");
    assert_eq!(result["changes_applied"], 3);

    // Verify each field
    let config = get_config(&http, node.admin_port).await;
    assert_eq!(
        config["hot"]["rate_limit"]["default_requests_per_second"],
        5000
    );
    assert_eq!(config["hot"]["gc"]["gc_interval_secs"], 120);
    assert_eq!(config["hot"]["logging"]["level"], "debug");
}

// ─── Test: PATCH with no changes returns 400 ───────────────────────────────

#[tokio::test]
async fn test_patch_config_no_changes_returns_error() {
    let nats = NatsContainer::start(14232)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-empty", &nats.url, 18083, 19094, 19093)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Empty body = no fields to update
    let body = json!({});
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "empty PATCH should return 400"
    );
}

// ─── Test: PATCH with invalid values returns 400 ───────────────────────────

#[tokio::test]
async fn test_patch_config_invalid_value_rejected() {
    let nats = NatsContainer::start(14233)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-invalid", &nats.url, 18084, 19095, 19094)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Read the config before the bad update
    let before = get_config(&http, node.admin_port).await;
    let before_level = before["hot"]["logging"]["level"]
        .as_str()
        .expect("log level should be a string")
        .to_string();

    // Try to set an invalid log level
    let body = json!({
        "logging_level": "verbose_and_nonexistent",
    });
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "invalid log level should be rejected"
    );

    // Verify the previous config is preserved (atomic swap — no partial update)
    let after = get_config(&http, node.admin_port).await;
    let after_level = after["hot"]["logging"]["level"]
        .as_str()
        .expect("log level should be a string");
    assert_eq!(
        after_level, before_level,
        "log level should be unchanged after rejected update"
    );
}

// ─── Test: DELETE /admin/config resets to defaults ─────────────────────────

#[tokio::test]
async fn test_delete_config_resets_to_defaults() {
    let nats = NatsContainer::start(14234)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-reset", &nats.url, 18085, 19096, 19095)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Apply a change first
    let body = json!({
        "rate_limit_default_rps": 7777,
        "logging_level": "debug",
    });
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify the change took effect
    let updated = get_config(&http, node.admin_port).await;
    assert_eq!(
        updated["hot"]["rate_limit"]["default_requests_per_second"],
        7777
    );

    // Reset
    let resp = delete_config(&http, node.admin_port).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let result: serde_json::Value = resp.json().await.expect("response should be JSON");
    assert_eq!(result["status"], "reset");

    // Verify the config is back to cold defaults
    let reset = get_config(&http, node.admin_port).await;
    let rps_after_reset = reset["hot"]["rate_limit"]["default_requests_per_second"]
        .as_u64()
        .expect("RPS should be a number");
    // The cold default comes from NodeConfig::default() which is 100
    assert_ne!(
        rps_after_reset, 7777,
        "RPS should no longer be the hot-reload value"
    );
}

// ─── Test: Hot config persists across node restart ─────────────────────────

#[tokio::test]
async fn test_config_persistence_across_restart() {
    let nats = NatsContainer::start(14235)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-persist", &nats.url, 18086, 19097, 19096)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Apply a distinctive change
    let body = json!({
        "rate_limit_default_rps": 4242,
    });
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify it took effect
    let updated = get_config(&http, node.admin_port).await;
    assert_eq!(
        updated["hot"]["rate_limit"]["default_requests_per_second"],
        4242
    );

    // Stop the node and extract its database + temp directory
    let (db_path, temp_dir) = node.extract_db();

    // Restart the node with the same database and admin port
    let node2 = NodeProcess::start_with_db_and_admin(
        "config-test-persist",
        &nats.url,
        18086,
        19097,
        19096,
        db_path,
        temp_dir,
    )
    .await
    .expect("node restart failed");

    sleep(Duration::from_secs(8)).await;

    // Verify the persisted override was applied
    let after_restart = get_config(&http, node2.admin_port).await;
    let rps_after = after_restart["hot"]["rate_limit"]["default_requests_per_second"]
        .as_u64()
        .expect("RPS should be a number");
    assert_eq!(
        rps_after, 4242,
        "persisted hot-config override should survive restart"
    );
}

// ─── Test: Log level change takes effect ───────────────────────────────────

#[tokio::test]
async fn test_log_level_change_takes_effect() {
    let nats = NatsContainer::start(14236)
        .await
        .expect("NATS start failed");
    let node =
        NodeProcess::start_with_admin("config-test-loglevel", &nats.url, 18087, 19098, 19097)
            .await
            .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Change log level to debug
    let body = json!({
        "logging_level": "debug",
    });
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify the hot config reflects the change
    let config = get_config(&http, node.admin_port).await;
    assert_eq!(config["hot"]["logging"]["level"], "debug");

    // Change it back to info
    let body = json!({
        "logging_level": "info",
    });
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let config = get_config(&http, node.admin_port).await;
    assert_eq!(config["hot"]["logging"]["level"], "info");
}

// ─── Test: GC interval change is accepted ──────────────────────────────────

#[tokio::test]
async fn test_gc_interval_change_accepted() {
    let nats = NatsContainer::start(14237)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-gc", &nats.url, 18088, 19099, 19098)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Change GC interval
    let body = json!({
        "gc_interval_secs": 60,
        "gc_disk_warning_threshold": 0.90,
    });
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let config = get_config(&http, node.admin_port).await;
    assert_eq!(config["hot"]["gc"]["gc_interval_secs"], 60);
    assert_eq!(config["hot"]["gc"]["disk_warning_threshold"], 0.90);
}

// ─── Test: Health check interval change is accepted ────────────────────────

#[tokio::test]
async fn test_health_interval_change_accepted() {
    let nats = NatsContainer::start(14238)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-health", &nats.url, 18089, 19100, 19099)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Change health check interval
    let body = json!({
        "health_check_interval_secs": 30,
        "health_default_idle_timeout_secs": 600,
    });
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let config = get_config(&http, node.admin_port).await;
    assert_eq!(config["hot"]["health"]["check_interval_secs"], 30);
    assert_eq!(config["hot"]["health"]["default_idle_timeout_secs"], 600);
}

// ─── Test: eBPF threshold changes are accepted ─────────────────────────────

#[tokio::test]
async fn test_ebpf_threshold_change_accepted() {
    let nats = NatsContainer::start(14239)
        .await
        .expect("NATS start failed");
    let node = NodeProcess::start_with_admin("config-test-ebpf", &nats.url, 18090, 19101, 19100)
        .await
        .expect("node start failed");

    let http = reqwest::Client::new();
    sleep(Duration::from_secs(2)).await;

    // Change eBPF thresholds
    let body = json!({
        "ebpf_fd_soft_limit": 4096,
        "ebpf_fd_hard_limit": 8192,
        "ebpf_syscall_rate_limit": 50000,
    });
    let resp = patch_config(&http, node.admin_port, &body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let config = get_config(&http, node.admin_port).await;
    assert_eq!(config["hot"]["ebpf"]["fd_soft_limit"], 4096);
    assert_eq!(config["hot"]["ebpf"]["fd_hard_limit"], 8192);
    assert_eq!(config["hot"]["ebpf"]["syscall_rate_limit"], 50000);
}
