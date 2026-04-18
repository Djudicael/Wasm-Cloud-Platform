use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Serialize, Deserialize, Debug)]
pub struct AuditEvent {
    pub timestamp: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_audit_log_append() {
        let path = "test_audit.log";
        let _ = fs::remove_file(path);

        let event = AuditEvent {
            timestamp: "2023-10-25T12:00:00Z".to_string(),
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
}
