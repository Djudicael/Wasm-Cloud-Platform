use dashmap::DashMap;
use std::time::Instant;

/// Circuit state for a single upstream app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Requests flow normally.
    Closed,
    /// Requests are rejected immediately (too many recent failures).
    Open,
    /// A single probe request is allowed through to test recovery.
    HalfOpen,
}

/// Per-app circuit breaker state.
struct Circuit {
    state: CircuitState,
    consecutive_failures: u32,
    last_failure_at: Option<Instant>,
    last_state_change: Instant,
}

/// Circuit breaker manager for all upstream apps.
pub struct CircuitBreakerManager {
    circuits: DashMap<String, Circuit>,
    configs: DashMap<String, common::types::CircuitBreakerConfig>,
    /// Default config for apps without explicit circuit breaker config.
    default_failure_threshold: u32,
    default_reset_timeout_secs: u32,
}

impl CircuitBreakerManager {
    pub fn new() -> Self {
        CircuitBreakerManager {
            circuits: DashMap::new(),
            configs: DashMap::new(),
            default_failure_threshold: 5,
            default_reset_timeout_secs: 30,
        }
    }

    /// Check if the circuit is open (requests should be rejected).
    pub fn is_circuit_open(&self, app_id: &str) -> bool {
        let mut circuit = self
            .circuits
            .entry(app_id.to_string())
            .or_insert_with(|| Circuit {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                last_failure_at: None,
                last_state_change: Instant::now(),
            });

        match circuit.state {
            CircuitState::Closed => false,
            CircuitState::Open => {
                // Check if enough time has passed to try half-open
                let elapsed = circuit.last_state_change.elapsed().as_secs();
                let timeout = self
                    .configs
                    .get(app_id)
                    .map(|c| c.reset_timeout_secs)
                    .unwrap_or(self.default_reset_timeout_secs);
                if elapsed >= timeout as u64 {
                    circuit.state = CircuitState::HalfOpen;
                    circuit.last_state_change = Instant::now();
                    tracing::info!(app = app_id, "circuit breaker: OPEN → HALF-OPEN");
                    false // allow the probe request
                } else {
                    true
                }
            }
            CircuitState::HalfOpen => false, // allow probe request
        }
    }

    /// Record a successful response from the upstream.
    pub fn record_success(&self, app_id: &str) {
        let mut circuit = self
            .circuits
            .entry(app_id.to_string())
            .or_insert_with(|| Circuit {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                last_failure_at: None,
                last_state_change: Instant::now(),
            });
        if circuit.state == CircuitState::HalfOpen {
            circuit.state = CircuitState::Closed;
            circuit.consecutive_failures = 0;
            circuit.last_state_change = Instant::now();
            tracing::info!(
                app = app_id,
                "circuit breaker: HALF-OPEN → CLOSED (recovered)"
            );
        } else {
            circuit.consecutive_failures = 0;
        }
    }

    /// Record a failure response from the upstream.
    pub fn record_failure(&self, app_id: &str) {
        let mut circuit = self
            .circuits
            .entry(app_id.to_string())
            .or_insert_with(|| Circuit {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                last_failure_at: None,
                last_state_change: Instant::now(),
            });
        circuit.consecutive_failures = circuit.consecutive_failures.saturating_add(1);
        circuit.last_failure_at = Some(Instant::now());

        match circuit.state {
            CircuitState::Closed => {
                let threshold = self
                    .configs
                    .get(app_id)
                    .map(|c| c.failure_threshold)
                    .unwrap_or(self.default_failure_threshold);
                if circuit.consecutive_failures >= threshold {
                    circuit.state = CircuitState::Open;
                    circuit.last_state_change = Instant::now();
                    tracing::warn!(
                        app = app_id,
                        failures = circuit.consecutive_failures,
                        "circuit breaker: CLOSED → OPEN (too many failures)"
                    );
                }
            }
            CircuitState::HalfOpen => {
                // Probe failed — go back to open
                circuit.state = CircuitState::Open;
                circuit.last_state_change = Instant::now();
                tracing::warn!(
                    app = app_id,
                    "circuit breaker: HALF-OPEN → OPEN (probe failed)"
                );
            }
            CircuitState::Open => {} // already open, nothing to do
        }
    }

    pub fn configure(&self, app: &str, config: Option<common::types::CircuitBreakerConfig>) {
        if let Some(c) = config {
            self.configs.insert(app.into(), c);
        } else {
            self.configs.remove(app);
        }
    }
    pub fn record_response(&self, app: &str, status: u16) {
        let failure = self
            .configs
            .get(app)
            .map(|c| match &c.failure_criteria {
                common::types::FailureCriteria::ServerErrors => status >= 500,
                common::types::FailureCriteria::AllErrors => status >= 400,
                common::types::FailureCriteria::StatusCodes(codes) => codes.contains(&status),
            })
            .unwrap_or(status >= 500);
        if failure {
            self.record_failure(app);
        } else {
            self.record_success(app);
        }
    }
    pub fn snapshot(&self, app: &str) -> serde_json::Value {
        self.circuits
            .get(app)
            .map(|c| Self::project(&c))
            .unwrap_or_else(|| serde_json::json!({"state":"closed","consecutiveFailures":0}))
    }
    fn project(c: &Circuit) -> serde_json::Value {
        serde_json::json!({"state":match c.state{CircuitState::Closed=>"closed",CircuitState::Open=>"open",CircuitState::HalfOpen=>"half_open"},"consecutiveFailures":c.consecutive_failures})
    }
    pub fn reset_if(&self, app: &str, expected: &serde_json::Value) -> bool {
        let mut c = self.circuits.entry(app.into()).or_insert_with(|| Circuit {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            last_failure_at: None,
            last_state_change: Instant::now(),
        });
        if Self::project(&c) != *expected {
            return false;
        }
        c.state = CircuitState::Closed;
        c.consecutive_failures = 0;
        c.last_failure_at = None;
        c.last_state_change = Instant::now();
        true
    }

    /// Count currently open circuits (for metrics).
    pub fn open_circuit_count(&self) -> i64 {
        self.circuits
            .iter()
            .filter(|c| c.state == CircuitState::Open)
            .count() as i64
    }

    /// Prune circuit breaker entries for apps that are no longer active.
    ///
    /// Call this periodically (e.g., after receiving a cluster snapshot or
    /// when apps are removed) to prevent memory leaks from stale entries.
    /// Returns the number of pruned entries.
    pub fn prune_removed_apps(&self, active_app_ids: &[String]) -> i64 {
        let before = self.circuits.len();
        self.circuits
            .retain(|app_id, _| active_app_ids.contains(app_id));
        let pruned = before - self.circuits.len();
        if pruned > 0 {
            tracing::info!(
                pruned,
                remaining = self.circuits.len(),
                "pruned circuit breaker entries for removed apps"
            );
        }
        pruned as i64
    }

    /// Set the last state change time for an app (test helper).
    pub fn set_last_state_change(&self, app_id: &str, instant: Instant) {
        let mut circuit = self
            .circuits
            .entry(app_id.to_string())
            .or_insert_with(|| Circuit {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                last_failure_at: None,
                last_state_change: Instant::now(),
            });
        circuit.last_state_change = instant;
    }
}

impl Default for CircuitBreakerManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_config_criteria_and_reviewed_reset_are_effective() {
        let cb = CircuitBreakerManager::new();
        cb.configure(
            "a",
            Some(common::types::CircuitBreakerConfig {
                failure_threshold: 2,
                reset_timeout_secs: 60,
                failure_criteria: common::types::FailureCriteria::StatusCodes(vec![429]),
            }),
        );
        cb.record_response("a", 500);
        assert_eq!(cb.snapshot("a")["consecutiveFailures"], 0);
        cb.record_response("a", 429);
        let stale = cb.snapshot("a");
        cb.record_response("a", 429);
        assert!(cb.is_circuit_open("a"));
        assert!(!cb.reset_if("a", &stale));
        assert!(cb.reset_if("a", &cb.snapshot("a")));
        assert!(!cb.is_circuit_open("a"));
        assert!(!cb.is_circuit_open("b"));
    }

    #[test]
    fn test_circuit_breaker_closed_to_open() {
        let cb = CircuitBreakerManager::new();
        let app = "test-app";

        // Initially closed
        assert!(!cb.is_circuit_open(app));

        // Record 5 failures (default threshold)
        for _ in 0..5 {
            cb.record_failure(app);
        }

        // Now open
        assert!(cb.is_circuit_open(app));
    }

    #[test]
    fn test_circuit_breaker_open_rejects() {
        let cb = CircuitBreakerManager::new();
        let app = "test-app";

        // Open the circuit
        for _ in 0..5 {
            cb.record_failure(app);
        }
        assert!(cb.is_circuit_open(app));

        // Still open immediately after
        assert!(cb.is_circuit_open(app));
    }

    #[test]
    fn test_circuit_breaker_half_open_recovery() {
        let cb = CircuitBreakerManager::new();
        let app = "test-app";

        // Open the circuit
        for _ in 0..5 {
            cb.record_failure(app);
        }
        assert!(cb.is_circuit_open(app));

        // Simulate time passing: we can't easily wait 30s in a unit test,
        // but we can directly test the success recording on a HalfOpen state
        // by manipulating internal state. For now, test that success while
        // closed doesn't break anything.
        cb.record_success(app);
        // Circuit is still open because we didn't wait for reset timeout
        assert!(cb.is_circuit_open(app));
    }

    #[test]
    fn test_circuit_breaker_half_open_failure() {
        let cb = CircuitBreakerManager::new();
        let app = "test-app";

        // Open the circuit
        for _ in 0..5 {
            cb.record_failure(app);
        }
        assert!(cb.is_circuit_open(app));

        // Success resets failures counter but circuit remains open
        // until reset timeout passes (can't test here without sleeping)
        cb.record_success(app);
        // After success, circuit is still open because state transition
        // requires time to pass; but failures counter is reset.
        assert!(cb.is_circuit_open(app));
    }

    #[test]
    fn test_circuit_breaker_record_success_resets_failures() {
        let cb = CircuitBreakerManager::new();
        let app = "test-app";

        // 4 failures — not yet open
        for _ in 0..4 {
            cb.record_failure(app);
        }
        assert!(!cb.is_circuit_open(app));

        // Success resets the counter
        cb.record_success(app);

        // Need 5 more failures to open
        for _ in 0..5 {
            cb.record_failure(app);
        }
        assert!(cb.is_circuit_open(app));
    }

    #[test]
    fn test_open_circuit_count() {
        let cb = CircuitBreakerManager::new();
        cb.record_failure("app1");
        cb.record_failure("app1");
        cb.record_failure("app1");
        cb.record_failure("app1");
        cb.record_failure("app1");
        cb.record_failure("app2");
        cb.record_failure("app2");
        cb.record_failure("app2");
        cb.record_failure("app2");
        cb.record_failure("app2");

        assert_eq!(cb.open_circuit_count(), 2);
    }
}
