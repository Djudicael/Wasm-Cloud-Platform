//! Structured test reports for chaos testing.
//!
//! Each chaos scenario produces a [`TestReport`] containing:
//!
//! - The overall result (`Pass`, `Fail`, or `SetupFailed`)
//! - Individual [`StepReport`] entries with timing and pass/fail status
//! - Total test duration
//! - Time To Recovery (TTR) — the primary metric for chaos tests
//!
//! Reports can be printed as human-readable summaries or exported as JSON
//! for CI integration. Reports are saved to `target/chaos-reports/` when
//! the `CHAOS_REPORT_DIR` environment variable is set (or defaults to that
//! path).
//!
//! ## Example Output
//!
//! ```text
//! ══════════════════════════════════════════════════════════════
//! CHAOS TEST: L1: Instance Crash Recovery
//! ══════════════════════════════════════════════════════════════
//!   ✅ setup_deploy_app (120ms) — ok
//!   ✅ wait_for_instance (340ms) — 340ms
//!   ✅ inject_crash (5ms) — L1: killed instance of chaos-app:v1
//!   ✅ verify_instance_removed (2100ms) — ok
//!   ✅ verify_instance_respawned (8500ms) — 8500ms
//!   ✅ verify_traffic_served (12ms) — 12ms
//! ──────────────────────────────────────────────────────────────
//! Result: ✅ PASS
//! TTR: 8500ms
//! ══════════════════════════════════════════════════════════════
//! ```

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A structured test report for a chaos scenario.
///
/// Contains the overall result, individual step results, total duration,
/// and the Time To Recovery (TTR) — the primary metric for chaos tests.
///
/// TTR is measured from the moment a failure is injected to the moment the
/// system returns to a healthy state:
///
/// ```text
/// TTR = Time(first healthy response after recovery) - Time(failure injected)
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    /// Name of the test scenario (e.g., "L1: Instance Crash Recovery").
    pub name: String,

    /// Overall result: pass, fail, or setup failed.
    pub result: TestResult,

    /// Individual step results with timing and pass/fail status.
    pub steps: Vec<StepReport>,

    /// Total test duration in seconds.
    pub total_duration_secs: f64,

    /// Time To Recovery in milliseconds (if applicable).
    ///
    /// This is the primary metric for chaos tests. It measures how long
    /// the system took to recover from the injected failure.
    pub ttr_ms: Option<u64>,

    /// Timestamp when the test started (ISO 8601 format).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    /// Timestamp when the test completed (ISO 8601 format).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,

    /// Unique identifier for this test run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Overall test result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TestResult {
    /// All steps passed — the system recovered correctly.
    Pass,
    /// One or more verification steps failed — recovery did not succeed.
    Fail,
    /// The test setup (cluster fixture, app deployment) failed before
    /// the failure could be injected.
    SetupFailed,
}

impl std::fmt::Display for TestResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestResult::Pass => write!(f, "PASS"),
            TestResult::Fail => write!(f, "FAIL"),
            TestResult::SetupFailed => write!(f, "SETUP_FAILED"),
        }
    }
}

/// Result of a single step within a chaos test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepReport {
    /// Name of the step (e.g., "inject_crash", "verify_healthy").
    pub name: String,
    /// Whether this step passed or failed.
    pub result: StepResultValue,
    /// Human-readable message (error description on failure, info on success).
    pub message: String,
    /// Duration of this step in milliseconds.
    pub duration_ms: u64,
}

/// Step result value: pass or fail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StepResultValue {
    Pass,
    Fail,
}

/// Internal representation of a step result before it is added to a report.
///
/// This is what chaos test scenarios produce for each step. It is converted
/// to a `StepReport` when added to a `TestReport`.
#[derive(Debug, Clone)]
pub struct StepResult {
    /// Name of the step.
    pub name: String,
    /// Whether this step passed.
    pub passed: bool,
    /// Human-readable message.
    pub message: String,
    /// Duration of this step.
    pub duration: Duration,
}

impl StepResult {
    /// Create a `StepResult` from a synchronous operation.
    ///
    /// The closure `f` is executed immediately. If it returns `Ok(T)`,
    /// the step passes and `T` is converted to a message via `IntoStepInfo`.
    /// If it returns `Err(String)`, the step fails with the error message.
    pub fn from_sync<F, T>(name: &str, f: F) -> Self
    where
        F: FnOnce() -> Result<T, String>,
        T: IntoStepInfo,
    {
        let start = std::time::Instant::now();
        match f() {
            Ok(val) => StepResult {
                name: name.to_string(),
                passed: true,
                message: val.into_info(),
                duration: start.elapsed(),
            },
            Err(e) => StepResult {
                name: name.to_string(),
                passed: false,
                message: e,
                duration: start.elapsed(),
            },
        }
    }

    /// Create a `StepResult` from an asynchronous operation.
    ///
    /// The async closure `f` is awaited. If it returns `Ok(T)`,
    /// the step passes and `T` is converted to a message via `IntoStepInfo`.
    /// If it returns `Err(String)`, the step fails with the error message.
    pub async fn from_async<F, Fut, T>(name: &str, f: F) -> Self
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
        T: IntoStepInfo,
    {
        let start = std::time::Instant::now();
        match f().await {
            Ok(val) => StepResult {
                name: name.to_string(),
                passed: true,
                message: val.into_info(),
                duration: start.elapsed(),
            },
            Err(e) => StepResult {
                name: name.to_string(),
                passed: false,
                message: e,
                duration: start.elapsed(),
            },
        }
    }

    /// Create a passing step result with a message.
    pub fn pass(name: &str, message: &str) -> Self {
        StepResult {
            name: name.to_string(),
            passed: true,
            message: message.to_string(),
            duration: Duration::ZERO,
        }
    }

    /// Create a failing step result with a message.
    pub fn fail(name: &str, message: &str) -> Self {
        StepResult {
            name: name.to_string(),
            passed: false,
            message: message.to_string(),
            duration: Duration::ZERO,
        }
    }

    /// Create a `StepResult` from a `Result<Duration, String>`.
    ///
    /// On `Ok(duration)`, the step passes with the duration formatted as
    /// milliseconds in the message. On `Err(msg)`, the step fails.
    ///
    /// This is the primary helper for verifier functions that return TTR.
    pub fn from_duration(name: &str, result: Result<Duration, String>) -> Self {
        match result {
            Ok(dur) => StepResult::pass(name, &format!("{}ms", dur.as_millis())),
            Err(e) => StepResult::fail(name, &e),
        }
    }

    /// Create a `StepResult` from a `Result<(), String>`.
    ///
    /// On `Ok(())`, the step passes with "ok". On `Err(msg)`, it fails.
    pub fn from_unit(name: &str, result: Result<(), String>) -> Self {
        match result {
            Ok(()) => StepResult::pass(name, "ok"),
            Err(e) => StepResult::fail(name, &e),
        }
    }

    /// Create a `StepResult` from a `Result<String, String>`.
    ///
    /// On `Ok(msg)`, the step passes with the message. On `Err(msg)`, it fails.
    pub fn from_string(name: &str, result: Result<String, String>) -> Self {
        match result {
            Ok(msg) => StepResult::pass(name, &msg),
            Err(e) => StepResult::fail(name, &e),
        }
    }
}

/// Trait for converting a step result value into a human-readable info string.
///
/// Implemented for common types that chaos test steps return:
/// - `()` → "ok"
/// - `String` → the string itself
/// - `Duration` → formatted as milliseconds
/// - `u64` → formatted as "count=N"
pub trait IntoStepInfo {
    fn into_info(self) -> String;
}

impl IntoStepInfo for () {
    fn into_info(self) -> String {
        "ok".to_string()
    }
}

impl IntoStepInfo for String {
    fn into_info(self) -> String {
        self
    }
}

impl IntoStepInfo for Duration {
    fn into_info(self) -> String {
        format!("{:.0}ms", self.as_millis())
    }
}

impl IntoStepInfo for u64 {
    fn into_info(self) -> String {
        format!("count={self}")
    }
}

impl IntoStepInfo for &str {
    fn into_info(self) -> String {
        self.to_string()
    }
}

impl IntoStepInfo for crate::injector::InjectionResult {
    fn into_info(self) -> String {
        self.description
    }
}

impl TestReport {
    /// Create a new test report with the given scenario name.
    ///
    /// The report starts in the `Pass` state. If any step fails, the
    /// overall result is automatically updated to `Fail`.
    pub fn new(name: &str) -> Self {
        TestReport {
            name: name.to_string(),
            result: TestResult::Pass,
            steps: Vec::new(),
            total_duration_secs: 0.0,
            ttr_ms: None,
            started_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            completed_at: None,
            run_id: Some(uuid::Uuid::new_v4().to_string()),
        }
    }

    /// Add a step result to the report.
    ///
    /// If the step failed, the overall report result is updated to `Fail`.
    /// Returns an owned copy of the added step so the caller can inspect
    /// its message without holding a borrow on `self`.
    pub fn add_step(&mut self, step: StepResult) -> StepReport {
        if !step.passed && self.result == TestResult::Pass {
            self.result = TestResult::Fail;
        }

        let report = StepReport {
            name: step.name.clone(),
            result: if step.passed {
                StepResultValue::Pass
            } else {
                StepResultValue::Fail
            },
            message: step.message.clone(),
            duration_ms: step.duration.as_millis() as u64,
        };

        self.steps.push(report.clone());
        report
    }

    /// Mark the report as a setup failure.
    ///
    /// Used when the cluster fixture or app deployment fails before the
    /// failure injection could begin.
    pub fn fail_setup(&mut self, reason: &str) {
        self.result = TestResult::SetupFailed;
        self.steps.push(StepReport {
            name: "setup".to_string(),
            result: StepResultValue::Fail,
            message: reason.to_string(),
            duration_ms: 0,
        });
    }

    /// Set the TTR (Time To Recovery) for this report.
    ///
    /// Typically called after the recovery verification step completes.
    pub fn set_ttr(&mut self, ttr: Duration) {
        self.ttr_ms = Some(ttr.as_millis() as u64);
    }

    /// Finalize the report: set the completion timestamp and total duration.
    ///
    /// Called automatically by `print_summary()` and `to_json()` if not
    /// called explicitly.
    pub fn finalize(&mut self) {
        if self.completed_at.is_none() {
            self.completed_at =
                Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        }
        if self.total_duration_secs == 0.0 {
            self.total_duration_secs = self
                .steps
                .iter()
                .map(|s| s.duration_ms as f64 / 1000.0)
                .sum();
        }
    }

    /// Check if the report represents a passing test.
    pub fn passed(&self) -> bool {
        self.result == TestResult::Pass
    }

    /// Check if the report represents a failed test.
    pub fn failed(&self) -> bool {
        matches!(self.result, TestResult::Fail | TestResult::SetupFailed)
    }

    /// Get the names of all failed steps.
    pub fn failed_steps(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|s| s.result == StepResultValue::Fail)
            .map(|s| s.name.as_str())
            .collect()
    }

    /// Get the total duration of all steps.
    pub fn total_duration(&self) -> Duration {
        Duration::from_millis(self.steps.iter().map(|s| s.duration_ms).sum())
    }

    /// Print a human-readable summary to stdout.
    ///
    /// ```text
    /// ══════════════════════════════════════════════════════════════
    /// CHAOS TEST: L1: Instance Crash Recovery
    /// ══════════════════════════════════════════════════════════════
    ///   ✅ setup_deploy_app (120ms) — ok
    ///   ✅ inject_crash (5ms) — L1: killed instance
    ///   ✅ verify_healthy (2100ms) — 2100ms
    /// ──────────────────────────────────────────────────────────────
    /// Result: ✅ PASS
    /// TTR: 2100ms
    /// ══════════════════════════════════════════════════════════════
    /// ```
    pub fn print_summary(&mut self) {
        self.finalize();

        println!("\n{}", "═".repeat(60));
        println!("CHAOS TEST: {}", self.name);
        println!("{}", "═".repeat(60));

        for step in &self.steps {
            let icon = match step.result {
                StepResultValue::Pass => "✅",
                StepResultValue::Fail => "❌",
            };
            println!(
                "  {icon} {} ({:.0}ms) — {}",
                step.name, step.duration_ms as f64, step.message
            );
        }

        let result_icon = match self.result {
            TestResult::Pass => "✅ PASS",
            TestResult::Fail => "❌ FAIL",
            TestResult::SetupFailed => "⚠️ SETUP FAILED",
        };
        println!("{}", "─".repeat(60));
        println!("Result: {result_icon}");

        if let Some(ttr) = self.ttr_ms {
            println!("TTR: {ttr}ms");
        }

        let total_ms: u64 = self.steps.iter().map(|s| s.duration_ms).sum();
        println!("Duration: {total_ms}ms");
        println!("{}", "═".repeat(60));
    }

    /// Export the report as pretty-printed JSON.
    ///
    /// Suitable for CI integration, artifact upload, or further analysis.
    /// The JSON schema includes all fields: name, result, steps, TTR,
    /// timestamps, and run ID.
    pub fn to_json(&mut self) -> String {
        self.finalize();
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Save the report to a file in the chaos reports directory.
    ///
    /// The directory is determined by the `CHAOS_REPORT_DIR` environment
    /// variable, defaulting to `target/chaos-reports/`. The file name
    /// includes the scenario name and run ID for uniqueness.
    pub fn save_to_file(&mut self) -> Result<String, String> {
        self.finalize();

        let report_dir = std::env::var("CHAOS_REPORT_DIR")
            .unwrap_or_else(|_| "target/chaos-reports".to_string());

        // Create the directory if it doesn't exist
        std::fs::create_dir_all(&report_dir)
            .map_err(|e| format!("failed to create report dir '{report_dir}': {e}"))?;

        // Sanitize the name for use as a file name
        let safe_name = self
            .name
            .replace([':', ' ', '/', '\\', '<', '>', '"', '|', '?', '*'], "_");
        let run_id = self.run_id.as_deref().unwrap_or("unknown");
        let filename = format!("{safe_name}_{run_id}.json");
        let path = std::path::Path::new(&report_dir).join(&filename);

        let json = self.to_json();
        std::fs::write(&path, json)
            .map_err(|e| format!("failed to write report to {}: {e}", path.display()))?;

        Ok(path.to_string_lossy().to_string())
    }
}

/// Print a summary of multiple test reports.
///
/// Displays a compact table with pass/fail counts and TTR for each scenario.
pub fn print_summary(reports: &[TestReport]) {
    println!("\n{}", "═".repeat(70));
    println!("CHAOS TEST SUITE — SUMMARY");
    println!("{}", "═".repeat(70));

    let mut pass_count = 0;
    let mut fail_count = 0;

    for report in reports {
        let icon = match report.result {
            TestResult::Pass => {
                pass_count += 1;
                "✅"
            }
            TestResult::Fail => {
                fail_count += 1;
                "❌"
            }
            TestResult::SetupFailed => {
                fail_count += 1;
                "⚠️"
            }
        };
        let ttr = report
            .ttr_ms
            .map(|t| format!("TTR={t}ms"))
            .unwrap_or_default();
        println!("  {icon} {} — {ttr}", report.name);
    }

    println!("{}", "─".repeat(70));
    println!("Total: {} passed, {} failed", pass_count, fail_count);
    println!("{}", "═".repeat(70));
}

/// Run all chaos test scenarios and return their reports.
///
/// This is the main entry point for running the full chaos test suite.
/// Each scenario is run sequentially, and reports are saved to the
/// chaos reports directory.
pub async fn run_all_and_save() -> Vec<TestReport> {
    let mut reports = Vec::new();

    // L1: Instance crash
    let r = crate::chaos::l1_instance_crash::test_l1_instance_crash_recovery().await;
    reports.push(r);

    // L2: Node restart
    let r = crate::chaos::l2_node_restart::test_l2_node_restart_recovery().await;
    reports.push(r);

    // L3: Redb corruption
    let r = crate::chaos::l3_redb_corruption::test_l3_redb_corruption_recovery().await;
    reports.push(r);

    // L4: Full rebuild
    let r = crate::chaos::l4_full_rebuild::test_l4_full_rebuild_recovery().await;
    reports.push(r);

    // L5: NATS partition
    let r = crate::chaos::l5_nats_partition::test_l5_nats_partition_recovery().await;
    reports.push(r);

    // L6: Multi-node failure
    let r = crate::chaos::l6_multi_node_failure::test_l6_multi_node_failure_recovery().await;
    reports.push(r);

    // Save all reports
    for report in &mut reports {
        if let Err(e) = report.save_to_file() {
            tracing::warn!(error = %e, "failed to save chaos report");
        }
    }

    print_summary(&reports);

    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_report_new() {
        let report = TestReport::new("L1: Instance Crash");
        assert_eq!(report.name, "L1: Instance Crash");
        assert_eq!(report.result, TestResult::Pass);
        assert!(report.steps.is_empty());
        assert!(report.ttr_ms.is_none());
    }

    #[test]
    fn test_report_add_passing_step() {
        let mut report = TestReport::new("test");
        report.add_step(StepResult::pass("step1", "ok"));
        assert_eq!(report.result, TestResult::Pass);
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].result, StepResultValue::Pass);
    }

    #[test]
    fn test_report_add_failing_step() {
        let mut report = TestReport::new("test");
        report.add_step(StepResult::fail("step1", "timeout"));
        assert_eq!(report.result, TestResult::Fail);
        assert_eq!(report.steps[0].result, StepResultValue::Fail);
    }

    #[test]
    fn test_report_fail_setup() {
        let mut report = TestReport::new("test");
        report.fail_setup("NATS container failed to start");
        assert_eq!(report.result, TestResult::SetupFailed);
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "setup");
    }

    #[test]
    fn test_report_set_ttr() {
        let mut report = TestReport::new("test");
        report.set_ttr(Duration::from_millis(8500));
        assert_eq!(report.ttr_ms, Some(8500));
    }

    #[test]
    fn test_report_passed_failed() {
        let mut report = TestReport::new("test");
        assert!(report.passed());
        assert!(!report.failed());

        report.add_step(StepResult::fail("step1", "error"));
        assert!(!report.passed());
        assert!(report.failed());
    }

    #[test]
    fn test_report_failed_steps() {
        let mut report = TestReport::new("test");
        report.add_step(StepResult::pass("step1", "ok"));
        report.add_step(StepResult::fail("step2", "timeout"));
        report.add_step(StepResult::pass("step3", "ok"));

        let failed = report.failed_steps();
        assert_eq!(failed, vec!["step2"]);
    }

    #[test]
    fn test_step_result_from_sync_ok() {
        let result = StepResult::from_sync("test", || Ok(()));
        assert!(result.passed);
        assert_eq!(result.message, "ok");
    }

    #[test]
    fn test_step_result_from_sync_err() {
        let result: StepResult = StepResult::from_sync("test", || -> Result<(), String> {
            Err("something broke".to_string())
        });
        assert!(!result.passed);
        assert_eq!(result.message, "something broke");
    }

    #[test]
    fn test_step_result_from_sync_with_duration() {
        let result = StepResult::from_sync("test", || Ok(Duration::from_millis(500)));
        assert!(result.passed);
        assert_eq!(result.message, "500ms");
    }

    #[test]
    fn test_step_result_from_sync_with_count() {
        let result = StepResult::from_sync("test", || Ok(42u64));
        assert!(result.passed);
        assert_eq!(result.message, "count=42");
    }

    #[test]
    fn test_step_result_from_sync_with_string() {
        let result = StepResult::from_sync("test", || Ok("hello".to_string()));
        assert!(result.passed);
        assert_eq!(result.message, "hello");
    }

    #[test]
    fn test_step_result_from_sync_with_str() {
        let result = StepResult::from_sync("test", || Ok("hello"));
        assert!(result.passed);
        assert_eq!(result.message, "hello");
    }

    #[tokio::test]
    async fn test_step_result_from_async_ok() {
        let result = StepResult::from_async("test", || async { Ok(()) }).await;
        assert!(result.passed);
        assert_eq!(result.message, "ok");
    }

    #[tokio::test]
    async fn test_step_result_from_async_err() {
        let result: StepResult =
            StepResult::from_async("test", || async { Err::<(), _>("async error".to_string()) })
                .await;
        assert!(!result.passed);
        assert_eq!(result.message, "async error");
    }

    #[test]
    fn test_report_to_json() {
        let mut report = TestReport::new("L1: Instance Crash Recovery");
        report.add_step(StepResult::pass("deploy", "ok"));
        report.add_step(StepResult::pass("inject", "L1: killed instance"));
        report.set_ttr(Duration::from_millis(5000));

        let json = report.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["name"], "L1: Instance Crash Recovery");
        assert_eq!(parsed["result"], "Pass");
        assert_eq!(parsed["ttr_ms"], 5000);
        assert_eq!(parsed["steps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_report_finalize() {
        let mut report = TestReport::new("test");
        assert!(report.completed_at.is_none());
        report.finalize();
        assert!(report.completed_at.is_some());
        assert!(report.total_duration_secs >= 0.0);
    }

    #[test]
    fn test_test_result_display() {
        assert_eq!(format!("{}", TestResult::Pass), "PASS");
        assert_eq!(format!("{}", TestResult::Fail), "FAIL");
        assert_eq!(format!("{}", TestResult::SetupFailed), "SETUP_FAILED");
    }

    #[test]
    fn test_print_summary() {
        let mut r1 = TestReport::new("L1: Instance Crash");
        r1.add_step(StepResult::pass("step1", "ok"));
        r1.set_ttr(Duration::from_millis(5000));

        let mut r2 = TestReport::new("L2: Node Restart");
        r2.add_step(StepResult::fail("step1", "timeout"));

        // Just verify it doesn't panic
        print_summary(&[r1, r2]);
    }

    #[test]
    fn test_report_save_to_file() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CHAOS_REPORT_DIR", dir.path().to_string_lossy().to_string());

        let mut report = TestReport::new("L1: Test");
        report.add_step(StepResult::pass("step1", "ok"));

        let path = report.save_to_file().unwrap();
        assert!(std::path::Path::new(&path).exists());

        // Verify the file contains valid JSON
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["name"], "L1: Test");

        // Clean up
        std::env::remove_var("CHAOS_REPORT_DIR");
    }

    #[test]
    fn test_into_step_info_impls() {
        assert_eq!(().into_info(), "ok");
        assert_eq!(String::from("hello").into_info(), "hello");
        assert_eq!(Duration::from_millis(1500).into_info(), "1500ms");
        assert_eq!(42u64.into_info(), "count=42");
        assert_eq!("hello".into_info(), "hello");
    }
}
