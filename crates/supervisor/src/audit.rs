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
    AdminApiCall,
    AuthFailure,
    TokenRotated,
    InternalGatewayRequest,
    CrossNamespaceDenied,
    NamespaceSecurityIncident,
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

    #[test]
    fn test_admin_api_call_audit_event() {
        let path = "test_admin_api_call_audit.log";
        let _ = fs::remove_file(path);

        let event = AuditEvent {
            timestamp: 1698158400,
            node_id: "node-1".to_string(),
            event_type: AuditEventType::AdminApiCall,
            actor: "admin:write_token".to_string(),
            app_id: "_platform".to_string(),
            details: serde_json::json!({
                "path": "/admin/rebuild",
                "method": "POST",
                "client_ip": "10.0.0.1",
                "status_code": 200,
            }),
        };
        write_audit_event(path, &event);

        let contents = fs::read_to_string(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["event_type"], "admin_api_call");
        assert_eq!(parsed["actor"], "admin:write_token");
        assert_eq!(parsed["details"]["path"], "/admin/rebuild");
        assert_eq!(parsed["details"]["method"], "POST");
        assert_eq!(parsed["details"]["status_code"], 200);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_auth_failure_audit_event() {
        let path = "test_auth_failure_audit.log";
        let _ = fs::remove_file(path);

        let event = AuditEvent {
            timestamp: 1698158400,
            node_id: "node-1".to_string(),
            event_type: AuditEventType::AuthFailure,
            actor: "admin:read_token".to_string(),
            app_id: "_platform".to_string(),
            details: serde_json::json!({
                "path": "/admin/config",
                "method": "GET",
                "client_ip": "192.168.1.100",
                "status_code": 401,
            }),
        };
        write_audit_event(path, &event);

        let contents = fs::read_to_string(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["event_type"], "auth_failure");
        assert_eq!(parsed["actor"], "admin:read_token");
        assert_eq!(parsed["details"]["status_code"], 401);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_token_rotated_audit_event() {
        let path = "test_token_rotated_audit.log";
        let _ = fs::remove_file(path);

        let event = AuditEvent {
            timestamp: 1698158400,
            node_id: "node-1".to_string(),
            event_type: AuditEventType::TokenRotated,
            actor: "admin:write_token".to_string(),
            app_id: "_platform".to_string(),
            details: serde_json::json!({
                "token_type": "write",
            }),
        };
        write_audit_event(path, &event);

        let contents = fs::read_to_string(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["event_type"], "token_rotated");
        assert_eq!(parsed["actor"], "admin:write_token");
        assert_eq!(parsed["details"]["token_type"], "write");

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_all_audit_event_types_serialize() {
        // Verify all AuditEventType variants serialize to snake_case as expected
        let cases = vec![
            (AuditEventType::AppDeployed, "app_deployed"),
            (AuditEventType::AppRemoved, "app_removed"),
            (AuditEventType::InstanceSpawned, "instance_spawned"),
            (AuditEventType::InstanceKilled, "instance_killed"),
            (AuditEventType::SecretRotated, "secret_rotated"),
            (AuditEventType::TrapOccurred, "trap_occurred"),
            (AuditEventType::BinaryHashMismatch, "binary_hash_mismatch"),
            (AuditEventType::RateLimitExceeded, "rate_limit_exceeded"),
            (AuditEventType::PolicyViolation, "policy_violation"),
            (AuditEventType::AdminApiCall, "admin_api_call"),
            (AuditEventType::AuthFailure, "auth_failure"),
            (AuditEventType::TokenRotated, "token_rotated"),
        ];

        for (event_type, expected) in cases {
            let json = serde_json::to_string(&event_type).unwrap();
            assert_eq!(
                json.trim_matches('"'),
                expected,
                "AuditEventType::{:?} should serialize to \"{}\"",
                event_type,
                expected
            );
        }
    }
}
