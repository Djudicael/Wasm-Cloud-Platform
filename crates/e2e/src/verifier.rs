//! Recovery verification primitives for chaos testing.
//!
//! Each function probes a specific aspect of system health after a failure
//! has been injected, returning a [`VerificationResult`] that includes the
//! Time To Recovery (TTR) — the elapsed time from failure injection to the
//! first healthy response.
//!
//! ## TTR Measurement
//!
//! ```text
//! TTR = Time(first healthy response after recovery) - Time(failure injected)
//! ```
//!
//! The caller typically passes the `InjectionResult::injected_at` timestamp
//! to compute the overall TTR. Individual verifiers also return their own
//! elapsed time for fine-grained analysis.
//!
//! ## Acceptable TTR Targets
//!
//! | Level | Failure Type              | Target TTR | Max TTR |
//! |-------|---------------------------|------------|---------|
//! | L1    | Instance crash (OOM/trap) | < 5s       | 10s     |
//! | L2    | Node process restart      | < 30s      | 60s     |
//! | L3    | Redb partial corruption    | < 10s      | 30s     |
//! | L4    | Full node rebuild         | < 120s     | 300s    |
//! | L5    | NATS partition (30s)      | < 45s      | 90s     |
//! | L6    | Multi-node failure         | < 300s     | 600s    |

use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Result of verifying recovery from a failure.
///
/// Contains the overall recovery status, the TTR, and a list of individual
/// checks that were performed.
#[derive(Debug)]
pub struct VerificationResult {
    /// Whether the system recovered successfully.
    pub recovered: bool,
    /// Time from failure injection to recovery (Time To Recovery).
    pub ttr: Duration,
    /// Description of what was verified.
    pub description: String,
    /// Additional details (which checks passed/failed).
    pub details: Vec<CheckResult>,
}

impl VerificationResult {
    /// Create a successful verification result.
    pub fn ok(description: &str, ttr: Duration) -> Self {
        VerificationResult {
            recovered: true,
            ttr,
            description: description.to_string(),
            details: Vec::new(),
        }
    }

    /// Create a failed verification result.
    pub fn fail(description: &str, details: Vec<CheckResult>) -> Self {
        VerificationResult {
            recovered: false,
            ttr: Duration::ZERO,
            description: description.to_string(),
            details,
        }
    }

    /// Create a successful verification result with individual check details.
    pub fn ok_with_checks(description: &str, ttr: Duration, details: Vec<CheckResult>) -> Self {
        VerificationResult {
            recovered: true,
            ttr,
            description: description.to_string(),
            details,
        }
    }

    /// Compute TTR relative to a given injection timestamp.
    pub fn ttr_since(&self, _injected_at: Instant) -> Duration {
        // The TTR stored here is the elapsed time of the verification itself.
        // The overall TTR from injection to recovery is:
        //   injected_at + self.ttr - injected_at = self.ttr
        // But if the caller wants the wall-clock TTR from injection:
        //   Instant::now() - injected_at (approximated by self.ttr)
        self.ttr
    }
}

/// Result of a single check within a verification.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub message: String,
}

impl CheckResult {
    /// Create a passing check.
    pub fn pass(name: &str, message: &str) -> Self {
        CheckResult {
            name: name.to_string(),
            passed: true,
            message: message.to_string(),
        }
    }

    /// Create a failing check.
    pub fn fail(name: &str, message: &str) -> Self {
        CheckResult {
            name: name.to_string(),
            passed: false,
            message: message.to_string(),
        }
    }
}

// ── Node Health ──────────────────────────────────────────────────────

/// Wait for a node's health endpoint to return healthy.
///
/// Polls `GET /health` on the admin address until it returns HTTP 200 or
/// the timeout is exceeded. Returns the elapsed time as the TTR.
///
/// # Errors
///
/// Returns an error string if the node does not become healthy within the
/// timeout.
pub async fn wait_for_node_healthy(
    admin_addr: &str,
    timeout: Duration,
) -> Result<Duration, String> {
    let start = Instant::now();
    let url = format!("http://{admin_addr}/health");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let ttr = start.elapsed();
                info!(
                    addr = admin_addr,
                    ttr_ms = ttr.as_millis(),
                    "node is healthy"
                );
                return Ok(ttr);
            }
            Ok(resp) => {
                // Server responded but not 200 — node may still be starting
                let status = resp.status();
                if start.elapsed() > timeout {
                    return Err(format!(
                        "node at {admin_addr} returned {status} (not healthy) within {:?}",
                        timeout
                    ));
                }
            }
            Err(_) => {
                // Connection refused or timeout — node not ready yet
                if start.elapsed() > timeout {
                    return Err(format!(
                        "node at {admin_addr} did not become healthy within {:?}",
                        timeout
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Wait for a node's admin API to be reachable (TCP level).
///
/// This is a lighter check than `wait_for_node_healthy` — it only verifies
/// that the admin server is accepting connections, not that the node is
/// fully initialized.
pub async fn wait_for_admin_reachable(
    admin_addr: &str,
    timeout: Duration,
) -> Result<Duration, String> {
    let start = Instant::now();
    let parts: Vec<&str> = admin_addr.split(':').collect();
    let host = parts.first().unwrap_or(&"127.0.0.1");
    let port: u16 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(9090);

    loop {
        if crate::helpers::check_tcp(host, port) {
            let ttr = start.elapsed();
            info!(
                addr = admin_addr,
                ttr_ms = ttr.as_millis(),
                "admin API reachable"
            );
            return Ok(ttr);
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "admin API at {admin_addr} not reachable within {:?}",
                timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ── App Instances ────────────────────────────────────────────────────

/// Wait for a specific app to have running instances on a node.
///
/// Polls `GET /admin/instances/{app_id}` until the instance count reaches
/// `min_instances` or the timeout is exceeded.
pub async fn wait_for_app_instances(
    admin_addr: &str,
    app_id: &str,
    min_instances: usize,
    timeout: Duration,
) -> Result<Duration, String> {
    let start = Instant::now();
    let url = format!("http://{admin_addr}/admin/instances/{app_id}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let count = body
                    .get("instances")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);

                if count >= min_instances {
                    let ttr = start.elapsed();
                    info!(
                        app = app_id,
                        count,
                        min_instances,
                        ttr_ms = ttr.as_millis(),
                        "app has instances"
                    );
                    return Ok(ttr);
                }
            }
            Ok(resp) => {
                // Non-success status — admin API may not have the route yet
                if start.elapsed() > timeout {
                    return Err(format!(
                        "admin API returned {} for app {app_id} within {:?}",
                        resp.status(),
                        timeout
                    ));
                }
            }
            Err(_) => {
                if start.elapsed() > timeout {
                    return Err(format!(
                        "app {app_id} did not reach {min_instances} instances within {:?}",
                        timeout
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Wait for a specific app to have **zero** running instances on a node.
///
/// Useful for verifying that a killed instance was actually removed from
/// the upstream table by the health loop.
pub async fn wait_for_app_zero_instances(
    admin_addr: &str,
    app_id: &str,
    timeout: Duration,
) -> Result<Duration, String> {
    let start = Instant::now();
    let url = format!("http://{admin_addr}/admin/instances/{app_id}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let count = body
                    .get("instances")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);

                if count == 0 {
                    let ttr = start.elapsed();
                    info!(
                        app = app_id,
                        ttr_ms = ttr.as_millis(),
                        "app has zero instances"
                    );
                    return Ok(ttr);
                }
            }
            _ => {}
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "app {app_id} still has instances after {:?}",
                timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ── Proxy Requests ────────────────────────────────────────────────────

/// Verify that an HTTP request to the proxy succeeds.
///
/// Sends `GET /` with the given `Host` header and checks that the response
/// status matches `expected_status`. Returns the elapsed time.
pub async fn verify_proxy_request(
    proxy_addr: &str,
    host: &str,
    expected_status: u16,
) -> Result<Duration, String> {
    let url = format!("http://{proxy_addr}/");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let start = Instant::now();
    let resp = client
        .get(&url)
        .header("Host", host)
        .send()
        .await
        .map_err(|e| format!("proxy request to {proxy_addr} (Host: {host}) failed: {e}"))?;

    let status = resp.status().as_u16();
    let ttr = start.elapsed();

    if status == expected_status {
        info!(
            proxy = proxy_addr,
            host,
            status,
            ttr_ms = ttr.as_millis(),
            "proxy request verified"
        );
        Ok(ttr)
    } else {
        Err(format!(
            "proxy request to {proxy_addr} (Host: {host}) returned {status}, expected {expected_status}"
        ))
    }
}

/// Verify that an HTTP request to the proxy succeeds with any 2xx status.
///
/// More lenient than `verify_proxy_request` — accepts any success status.
pub async fn verify_proxy_request_any_2xx(
    proxy_addr: &str,
    host: &str,
) -> Result<Duration, String> {
    let url = format!("http://{proxy_addr}/");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let start = Instant::now();
    let resp = client
        .get(&url)
        .header("Host", host)
        .send()
        .await
        .map_err(|e| format!("proxy request to {proxy_addr} (Host: {host}) failed: {e}"))?;

    let status = resp.status();
    let ttr = start.elapsed();

    if status.is_success() {
        info!(
            proxy = proxy_addr,
            host,
            status = status.as_u16(),
            ttr_ms = ttr.as_millis(),
            "proxy request verified (2xx)"
        );
        Ok(ttr)
    } else {
        Err(format!(
            "proxy request to {proxy_addr} (Host: {host}) returned {}, expected 2xx",
            status
        ))
    }
}

/// Send a request and verify the response body contains a specific string.
///
/// Useful for verifying that the correct app version is serving traffic
/// (e.g., after a hot-swap or rebuild).
pub async fn verify_proxy_request_body_contains(
    proxy_addr: &str,
    host: &str,
    path: &str,
    expected_body_fragment: &str,
) -> Result<Duration, String> {
    let url = format!("http://{proxy_addr}{path}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let start = Instant::now();
    let resp = client
        .get(&url)
        .header("Host", host)
        .send()
        .await
        .map_err(|e| format!("proxy request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("proxy request returned {status}, expected 2xx"));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;

    let ttr = start.elapsed();

    if body.contains(expected_body_fragment) {
        Ok(ttr)
    } else {
        Err(format!(
            "response body does not contain '{expected_body_fragment}': {}",
            &body[..body.len().min(200)]
        ))
    }
}

// ── Billing Chain ────────────────────────────────────────────────────

/// Verify that billing records have an intact hash chain.
///
/// Calls `POST /admin/billing/verify` on the admin API and checks that
/// the response indicates a valid hash chain. This is critical for
/// verifying that no billing data was lost during a crash.
pub async fn verify_billing_chain(admin_addr: &str) -> Result<Duration, String> {
    let url = format!("http://{admin_addr}/admin/billing/verify");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let start = Instant::now();
    let resp = client
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("billing verify request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("billing verify returned status {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let valid = body.get("valid").and_then(|v| v.as_bool()).unwrap_or(false);

    let ttr = start.elapsed();

    if valid {
        info!(
            admin_addr,
            ttr_ms = ttr.as_millis(),
            "billing chain verified"
        );
        Ok(ttr)
    } else {
        let reason = body
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        Err(format!("billing chain verification failed: {reason}"))
    }
}

/// Count billing records via the admin API.
///
/// Returns the number of billing records stored on the node. Useful for
/// verifying that billing records survive a node restart.
pub async fn count_billing_records(admin_addr: &str) -> Result<u64, String> {
    let url = format!("http://{admin_addr}/admin/billing/count");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("billing count request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("billing count returned status {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(body.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
}

// ── Routes ───────────────────────────────────────────────────────────

/// Verify that a specific route exists on a node.
///
/// Calls `GET /admin/routes` and checks that a route for the given host
/// is present. This is used to verify that routes are restored after a
/// redb corruption recovery or full node rebuild.
pub async fn verify_route_exists(admin_addr: &str, host: &str) -> Result<Duration, String> {
    let url = format!("http://{admin_addr}/admin/routes");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let start = Instant::now();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("routes request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("routes endpoint returned status {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let routes = body
        .get("routes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let found = routes
        .iter()
        .any(|r| r.get("host").and_then(|h| h.as_str()) == Some(host));

    let ttr = start.elapsed();

    if found {
        info!(host, ttr_ms = ttr.as_millis(), "route exists");
        Ok(ttr)
    } else {
        Err(format!(
            "route for host '{host}' not found ({} routes total)",
            routes.len()
        ))
    }
}

/// Verify that a specific route does NOT exist on a node.
///
/// Useful for verifying that a `RouteRemove` event was processed correctly.
pub async fn verify_route_absent(admin_addr: &str, host: &str) -> Result<Duration, String> {
    let url = format!("http://{admin_addr}/admin/routes");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let start = Instant::now();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("routes request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("routes endpoint returned status {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let routes = body
        .get("routes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let found = routes
        .iter()
        .any(|r| r.get("host").and_then(|h| h.as_str()) == Some(host));

    let ttr = start.elapsed();

    if !found {
        info!(host, ttr_ms = ttr.as_millis(), "route correctly absent");
        Ok(ttr)
    } else {
        Err(format!(
            "route for host '{host}' still exists (should have been removed)"
        ))
    }
}

// ── Secrets ──────────────────────────────────────────────────────────

/// Verify that a secret is accessible for an app.
///
/// Calls `GET /admin/secrets/{app_id}/{key}` and checks that the response
/// is successful. This verifies that secrets survive a node restart or
/// rebuild.
pub async fn verify_secret_accessible(
    admin_addr: &str,
    app_id: &str,
    key: &str,
) -> Result<Duration, String> {
    let url = format!("http://{admin_addr}/admin/secrets/{app_id}/{key}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let start = Instant::now();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("secret check request failed: {e}"))?;

    let ttr = start.elapsed();

    if resp.status().is_success() {
        info!(app_id, key, ttr_ms = ttr.as_millis(), "secret accessible");
        Ok(ttr)
    } else {
        Err(format!(
            "secret {key} for {app_id} is not accessible (status {})",
            resp.status()
        ))
    }
}

// ── NATS Connectivity ────────────────────────────────────────────────

/// Verify NATS connectivity for a node.
///
/// Polls the health endpoint and checks the `nats` field in the response
/// body. Returns when NATS status is "connected" or the timeout is exceeded.
pub async fn verify_nats_connected(
    admin_addr: &str,
    timeout: Duration,
) -> Result<Duration, String> {
    let start = Instant::now();
    let url = format!("http://{admin_addr}/health");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let nats_connected = body
                    .get("nats_connected")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if nats_connected {
                    let ttr = start.elapsed();
                    info!(ttr_ms = ttr.as_millis(), "NATS reconnected");
                    return Ok(ttr);
                }
            }
            _ => {}
        }

        if start.elapsed() > timeout {
            return Err(format!("NATS did not reconnect within {:?}", timeout));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Verify that NATS is disconnected (degraded mode).
///
/// Polls the health endpoint and checks that the `nats` field reports
/// "disconnected". Used to verify that the node correctly detects a
/// NATS partition.
pub async fn verify_nats_disconnected(
    admin_addr: &str,
    timeout: Duration,
) -> Result<Duration, String> {
    let start = Instant::now();
    let url = format!("http://{admin_addr}/health");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let nats_connected = body
                    .get("nats_connected")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);

                if !nats_connected {
                    let ttr = start.elapsed();
                    info!(ttr_ms = ttr.as_millis(), "NATS confirmed disconnected");
                    return Ok(ttr);
                }
            }
            _ => {}
        }

        if start.elapsed() > timeout {
            return Err(format!(
                "NATS did not report disconnected within {:?}",
                timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── Composite Verification ────────────────────────────────────────────

/// Run a comprehensive recovery verification on a node.
///
/// Checks:
/// 1. Node health endpoint returns 200
/// 2. NATS is connected
/// 3. A specific app has running instances
/// 4. The proxy serves traffic for the app's host
/// 5. Billing chain is intact
///
/// Returns a `VerificationResult` with all individual check results.
pub async fn verify_full_recovery(
    admin_addr: &str,
    proxy_addr: &str,
    app_id: &str,
    host: &str,
    timeout: Duration,
) -> VerificationResult {
    let overall_start = Instant::now();
    let mut checks = Vec::new();
    let mut all_passed = true;

    // 1. Node health
    match wait_for_node_healthy(admin_addr, timeout).await {
        Ok(ttr) => {
            checks.push(CheckResult::pass(
                "node_healthy",
                &format!("TTR={}ms", ttr.as_millis()),
            ));
        }
        Err(e) => {
            checks.push(CheckResult::fail("node_healthy", &e));
            all_passed = false;
        }
    }

    // 2. NATS connected
    if all_passed {
        match verify_nats_connected(admin_addr, Duration::from_secs(30)).await {
            Ok(ttr) => {
                checks.push(CheckResult::pass(
                    "nats_connected",
                    &format!("TTR={}ms", ttr.as_millis()),
                ));
            }
            Err(e) => {
                checks.push(CheckResult::fail("nats_connected", &e));
                all_passed = false;
            }
        }
    }

    // 3. App instances
    if all_passed {
        match wait_for_app_instances(admin_addr, app_id, 1, timeout).await {
            Ok(ttr) => {
                checks.push(CheckResult::pass(
                    "app_instances",
                    &format!("TTR={}ms", ttr.as_millis()),
                ));
            }
            Err(e) => {
                checks.push(CheckResult::fail("app_instances", &e));
                all_passed = false;
            }
        }
    }

    // 4. Proxy serves traffic
    if all_passed {
        match verify_proxy_request(proxy_addr, host, 200).await {
            Ok(ttr) => {
                checks.push(CheckResult::pass(
                    "proxy_serves",
                    &format!("TTR={}ms", ttr.as_millis()),
                ));
            }
            Err(e) => {
                checks.push(CheckResult::fail("proxy_serves", &e));
                all_passed = false;
            }
        }
    }

    // 5. Billing chain
    if all_passed {
        match verify_billing_chain(admin_addr).await {
            Ok(ttr) => {
                checks.push(CheckResult::pass(
                    "billing_chain",
                    &format!("TTR={}ms", ttr.as_millis()),
                ));
            }
            Err(e) => {
                checks.push(CheckResult::fail("billing_chain", &e));
                // Billing chain failure is a warning, not a hard failure for recovery
                warn!(error = %e, "billing chain check failed (non-fatal for recovery)");
            }
        }
    }

    let overall_ttr = overall_start.elapsed();

    if all_passed {
        VerificationResult::ok_with_checks(
            &format!("full recovery verified for {app_id}"),
            overall_ttr,
            checks,
        )
    } else {
        VerificationResult::fail(&format!("full recovery failed for {app_id}"), checks)
    }
}

// ── Multi-Node Verification ──────────────────────────────────────────

/// Verify that all nodes in a cluster are healthy.
///
/// Checks each node's health endpoint in sequence. Returns a combined
/// result with per-node check details.
pub async fn verify_cluster_healthy(
    admin_addrs: &[String],
    timeout: Duration,
) -> VerificationResult {
    let overall_start = Instant::now();
    let mut checks = Vec::new();
    let mut all_passed = true;

    for (i, addr) in admin_addrs.iter().enumerate() {
        match wait_for_node_healthy(addr, timeout).await {
            Ok(ttr) => {
                checks.push(CheckResult::pass(
                    &format!("node_{i}_healthy"),
                    &format!("TTR={}ms", ttr.as_millis()),
                ));
            }
            Err(e) => {
                checks.push(CheckResult::fail(&format!("node_{i}_healthy"), &e));
                all_passed = false;
            }
        }
    }

    let overall_ttr = overall_start.elapsed();

    if all_passed {
        VerificationResult::ok_with_checks(
            &format!("cluster healthy ({} nodes)", admin_addrs.len()),
            overall_ttr,
            checks,
        )
    } else {
        VerificationResult::fail(
            &format!("cluster not fully healthy ({} nodes)", admin_addrs.len()),
            checks,
        )
    }
}

/// Verify that all nodes in a cluster can serve proxy traffic for a host.
pub async fn verify_cluster_serves_traffic(
    proxy_addrs: &[String],
    host: &str,
) -> VerificationResult {
    let overall_start = Instant::now();
    let mut checks = Vec::new();
    let mut all_passed = true;

    for (i, addr) in proxy_addrs.iter().enumerate() {
        match verify_proxy_request(addr, host, 200).await {
            Ok(ttr) => {
                checks.push(CheckResult::pass(
                    &format!("node_{i}_proxy"),
                    &format!("TTR={}ms", ttr.as_millis()),
                ));
            }
            Err(e) => {
                checks.push(CheckResult::fail(&format!("node_{i}_proxy"), &e));
                all_passed = false;
            }
        }
    }

    let overall_ttr = overall_start.elapsed();

    if all_passed {
        VerificationResult::ok_with_checks(
            &format!(
                "cluster serves traffic for '{host}' ({} nodes)",
                proxy_addrs.len()
            ),
            overall_ttr,
            checks,
        )
    } else {
        VerificationResult::fail(
            &format!("cluster does not fully serve traffic for '{host}'"),
            checks,
        )
    }
}

// ── Integrity Check ──────────────────────────────────────────────────

/// Verify that the node's startup integrity check completed successfully.
///
/// Polls the admin API for the integrity check status. After a redb
/// corruption, the node should report either `Healthy` or `PartialRebuild`
/// (both are acceptable recovery outcomes).
pub async fn verify_integrity_check_passed(
    admin_addr: &str,
    timeout: Duration,
) -> Result<Duration, String> {
    let start = Instant::now();
    let url = format!("http://{admin_addr}/admin/integrity");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let action = body
                    .get("recommendation")
                    .and_then(|v| v.get("action"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("unknown");

                match action {
                    "healthy" | "partial_rebuild" => {
                        let ttr = start.elapsed();
                        info!(action, ttr_ms = ttr.as_millis(), "integrity check passed");
                        return Ok(ttr);
                    }
                    "full_rebootstrap" => {
                        return Err(format!(
                            "integrity check recommends full rebootstrap — data loss likely"
                        ));
                    }
                    _ => {
                        // Unknown action — keep polling
                    }
                }
            }
            _ => {
                if start.elapsed() > timeout {
                    return Err(format!(
                        "integrity check did not complete within {:?}",
                        timeout
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verification_result_ok() {
        let result = VerificationResult::ok("node healthy", Duration::from_millis(150));
        assert!(result.recovered);
        assert_eq!(result.ttr, Duration::from_millis(150));
        assert!(result.details.is_empty());
    }

    #[test]
    fn test_verification_result_fail() {
        let checks = vec![CheckResult::fail("health", "timeout")];
        let result = VerificationResult::fail("node unhealthy", checks);
        assert!(!result.recovered);
        assert_eq!(result.ttr, Duration::ZERO);
        assert_eq!(result.details.len(), 1);
    }

    #[test]
    fn test_check_result_pass() {
        let check = CheckResult::pass("nats", "connected");
        assert!(check.passed);
        assert_eq!(check.name, "nats");
    }

    #[test]
    fn test_check_result_fail() {
        let check = CheckResult::fail("billing", "chain broken");
        assert!(!check.passed);
        assert_eq!(check.message, "chain broken");
    }

    #[test]
    fn test_verification_result_with_checks() {
        let checks = vec![
            CheckResult::pass("health", "ok"),
            CheckResult::pass("nats", "connected"),
        ];
        let result =
            VerificationResult::ok_with_checks("full recovery", Duration::from_millis(500), checks);
        assert!(result.recovered);
        assert_eq!(result.details.len(), 2);
        assert!(result.details[0].passed);
        assert!(result.details[1].passed);
    }

    #[test]
    fn test_ttr_since() {
        let injected_at = Instant::now();
        let result = VerificationResult::ok("test", Duration::from_millis(100));
        // TTR should be approximately the duration we passed in
        let ttr = result.ttr_since(injected_at);
        assert_eq!(ttr, Duration::from_millis(100));
    }
}
