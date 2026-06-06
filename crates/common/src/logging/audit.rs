use serde::{Deserialize, Serialize};

/// An audit log record for security-sensitive operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogRecord {
    pub timestamp: String,
    pub log_type: String,
    pub action: String,
    pub actor: String,
    pub node_id: String,
    pub app_id: Option<String>,
    pub success: bool,
    pub error: Option<String>,
    pub source_ip: Option<String>,
    pub details: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub enum AuditOutput {
    File {
        path: std::path::PathBuf,
    },
    Nats {
        client: async_nats::Client,
        subject: String,
    },
    Stderr,
}

/// A dedicated writer for audit logs.
#[derive(Debug, Clone)]
pub struct AuditLogger {
    node_id: String,
    tx: tokio::sync::mpsc::Sender<AuditLogRecord>,
    dropped_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl AuditLogger {
    pub fn start(output: AuditOutput) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AuditLogRecord>(1000);

        tokio::spawn(async move {
            while let Some(record) = rx.recv().await {
                let json = serde_json::to_string(&record).unwrap_or_default();
                match &output {
                    AuditOutput::File { path } => {
                        use std::io::Write;
                        if let Ok(mut file) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                        {
                            let _ = writeln!(file, "{}", json);
                        }
                    }
                    AuditOutput::Nats { client, subject } => {
                        let _ = client
                            .publish(subject.clone(), json.into_bytes().into())
                            .await;
                    }
                    AuditOutput::Stderr => {
                        eprintln!("{}", json);
                    }
                }
            }
        });

        AuditLogger {
            node_id: "unknown".to_string(),
            tx,
            dropped_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn set_node_id(&mut self, node_id: String) {
        self.node_id = node_id;
    }

    pub fn record(&self, action: &str, actor: &str, success: bool) {
        let record = AuditLogRecord {
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            log_type: "audit".to_string(),
            action: action.to_string(),
            actor: actor.to_string(),
            node_id: self.node_id.clone(),
            app_id: None,
            success,
            error: None,
            source_ip: None,
            details: serde_json::Map::new(),
        };
        if self.tx.try_send(record).is_err() {
            let dropped = self
                .dropped_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if dropped == 1 || dropped.is_multiple_of(1000) {
                tracing::warn!("audit log record dropped ({} total dropped)", dropped);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_detailed(
        &self,
        action: &str,
        actor: &str,
        app_id: Option<&str>,
        success: bool,
        error: Option<&str>,
        source_ip: Option<&str>,
        details: serde_json::Map<String, serde_json::Value>,
    ) {
        let record = AuditLogRecord {
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            log_type: "audit".to_string(),
            action: action.to_string(),
            actor: actor.to_string(),
            node_id: self.node_id.clone(),
            app_id: app_id.map(|s| s.to_string()),
            success,
            error: error.map(|s| s.to_string()),
            source_ip: source_ip.map(|s| s.to_string()),
            details,
        };
        if self.tx.try_send(record).is_err() {
            let dropped = self
                .dropped_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if dropped == 1 || dropped.is_multiple_of(1000) {
                tracing::warn!("audit log record dropped ({} total dropped)", dropped);
            }
        }
    }
}
