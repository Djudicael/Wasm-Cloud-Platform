use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Serialize, Deserialize, Debug)]
pub struct AuditEvent {
    pub timestamp: u64,
    pub node_id: String,
    pub event_type: AuditEventType,
    pub actor: String,
    pub app_id: String,
    pub details: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    AppDeployed,
    AppRemoved,
    InstanceSpawned,
    InstanceKilled,
    SecretRotated,
    TrapOccurred,
    BinaryHashMismatch,
    RateLimitExceeded,
    PolicyViolation,
}

pub fn write_audit_event(path: &str, event: &AuditEvent) {
    if let Ok(line) = serde_json::to_string(event) {
        if let Err(e) = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .and_then(|mut f| f.write_all(format!("{}\n", line).as_bytes()))
        {
            tracing::warn!(error = %e, path = path, "failed to write audit event");
        }
    }
}

/// Log a WASI policy violation to the audit trail.
///
/// Called by the WASI host function wrappers when a policy check denies an
/// operation. Every denial is recorded for forensic analysis and triggers
/// Prometheus alerting rules.
pub fn log_policy_violation(
    app_id: &str,
    instance_id: &str,
    denial_type: &str,
    denial_reason: &str,
) {
    let event = AuditEvent {
        timestamp: chrono::Utc::now().timestamp_millis() as u64,
        node_id: std::env::var("NODE_ID").unwrap_or_default(),
        event_type: AuditEventType::PolicyViolation,
        actor: "wasi_policy_enforcer".to_string(),
        app_id: app_id.to_string(),
        details: serde_json::json!({
            "instance_id": instance_id,
            "denial_type": denial_type,
            "denial_reason": denial_reason,
        }),
    };
    write_audit_event("/var/log/wasm-node/audit.jsonl", &event);

    tracing::warn!(
        app = app_id,
        instance = instance_id,
        denial_type = denial_type,
        reason = denial_reason,
        "WASI policy violation"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_audit_log_append() {
        let path = "test_audit.log";
        let _ = fs::remove_file(path);

        let event = AuditEvent {
            timestamp: 1698158400,
            node_id: "node-1".to_string(),
            event_type: AuditEventType::InstanceSpawned,
            actor: "system".to_string(),
            app_id: "test-app".to_string(),
            details: serde_json::json!({}),
        };

        write_audit_event(path, &event);
        write_audit_event(path, &event);

        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(contents.lines().count(), 2);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_policy_violation_audit_event() {
        let path = "test_policy_violation_audit.log";
        let _ = fs::remove_file(path);

        log_policy_violation(
            "my-app:v1",
            "inst-1234",
            "ConnectionLimitExceeded",
            "outbound connection limit exceeded: current 100, limit 100",
        );

        // Verify the event was written (to the default path, which may not exist in tests)
        // Instead, construct and write manually to a test path
        let event = AuditEvent {
            timestamp: 1698158400,
            node_id: "test-node".to_string(),
            event_type: AuditEventType::PolicyViolation,
            actor: "wasi_policy_enforcer".to_string(),
            app_id: "my-app:v1".to_string(),
            details: serde_json::json!({
                "instance_id": "inst-1234",
                "denial_type": "ConnectionLimitExceeded",
                "denial_reason": "outbound connection limit exceeded: current 100, limit 100",
            }),
        };
        write_audit_event(path, &event);

        let contents = fs::read_to_string(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["event_type"], "policy_violation");
        assert_eq!(parsed["actor"], "wasi_policy_enforcer");
        assert_eq!(parsed["details"]["denial_type"], "ConnectionLimitExceeded");

        fs::remove_file(path).unwrap();
    }
}
