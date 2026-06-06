use serde::{Deserialize, Serialize};
use tracing::{field::Field, field::Visit, Event, Subscriber};
use tracing_subscriber::fmt::{format::Writer, FmtContext, FormatEvent, FormatFields};

/// The standard envelope for all node-level structured log records.
/// This is what the JSON formatter emits - one JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLogRecord {
    pub timestamp: String,
    pub level: String,
    pub target: String,
    pub span: Option<String>,
    pub message: String,
    pub node_id: String,
    pub app_id: Option<String>,
    pub instance_id: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub fields: serde_json::Map<String, serde_json::Value>,
    pub source_file: Option<String>,
    pub source_line: Option<u32>,
}

/// Configuration for the structured logging subsystem.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub format: LogFormat,
    pub output: LogOutput,
    pub default_level: String,
    pub module_levels: std::collections::HashMap<String, String>,
    pub sampling_enabled: bool,
    pub info_sample_rate: u64,
    pub debug_sample_rate: u64,
    pub trace_sample_rate: u64,
    pub node_id: String,
    pub include_source: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, Clone)]
pub enum LogOutput {
    Stdout,
    Stderr,
    File { path: std::path::PathBuf },
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            format: LogFormat::Json,
            output: LogOutput::Stdout,
            default_level: "info".to_string(),
            module_levels: std::collections::HashMap::new(),
            sampling_enabled: false,
            info_sample_rate: 1,
            debug_sample_rate: 10,
            trace_sample_rate: 100,
            node_id: "unknown".to_string(),
            include_source: cfg!(debug_assertions),
        }
    }
}

/// A custom JSON formatter that produces `NodeLogRecord`-compatible output.
pub struct NodeJsonFormatter {
    node_id: String,
    include_source: bool,
}

impl NodeJsonFormatter {
    pub fn new(node_id: String, include_source: bool) -> Self {
        NodeJsonFormatter {
            node_id,
            include_source,
        }
    }
}

impl<S, N> FormatEvent<S, N> for NodeJsonFormatter
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let metadata = event.metadata();
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);

        let mut record = serde_json::Map::new();
        record.insert(
            "timestamp".to_string(),
            serde_json::Value::String(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            ),
        );
        record.insert(
            "level".to_string(),
            serde_json::Value::String(metadata.level().to_string()),
        );
        record.insert(
            "target".to_string(),
            serde_json::Value::String(metadata.target().to_string()),
        );

        if let Some(span) = ctx.lookup_current() {
            record.insert(
                "span".to_string(),
                serde_json::Value::String(span.name().to_string()),
            );
        }

        record.insert(
            "node_id".to_string(),
            serde_json::Value::String(self.node_id.clone()),
        );

        let message = visitor
            .fields
            .remove("message")
            .unwrap_or_else(|| serde_json::Value::String(String::new()));
        record.insert("message".to_string(), message);

        for key in &["app_id", "instance_id", "trace_id", "span_id"] {
            if let Some(value) = visitor.fields.remove(*key) {
                record.insert(key.to_string(), value);
            }
        }

        if !visitor.fields.is_empty() {
            record.insert(
                "fields".to_string(),
                serde_json::Value::Object(visitor.fields),
            );
        }

        if self.include_source {
            if let Some(file) = metadata.file() {
                record.insert(
                    "source_file".to_string(),
                    serde_json::Value::String(file.to_string()),
                );
            }
            if let Some(line) = metadata.line() {
                record.insert(
                    "source_line".to_string(),
                    serde_json::Value::Number(line.into()),
                );
            }
        }

        writeln!(writer, "{}", serde_json::Value::Object(record))
    }
}

/// Collects fields from a tracing event into a JSON object.
#[derive(Default)]
pub(crate) struct FieldCollector {
    pub(crate) fields: serde_json::Map<String, serde_json::Value>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if value.is_finite() {
            if let Some(n) = serde_json::Number::from_f64(value) {
                self.fields
                    .insert(field.name().to_string(), serde_json::Value::Number(n));
            } else {
                self.fields.insert(
                    field.name().to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{:?}", value)),
        );
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
}
