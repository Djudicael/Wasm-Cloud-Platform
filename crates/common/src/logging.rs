//! Structured logging for the Wasm Cloud Platform.
//!
//! This module provides:
//! - `NodeLogRecord` - the canonical JSON schema for all node-level logs
//! - `NodeJsonFormatter` - a custom `tracing-subscriber` formatter emitting that schema
//! - `init_logging()` - one-shot initialisation of the global subscriber
//! - `LogReloadHandle` - runtime log-level changes without restart
//! - `SamplingLayer` - rate-limits INFO/DEBUG/TRACE while keeping WARN/ERROR at 100 %
//! - `AuditLogger` - non-blocking, separate-channel audit events
//! - `RotatingFileWriter` - size-based log rotation with optional gzip
//! - Configuration types (`LoggingConfig`, `LogForwarderConfig`, ...)

use serde::{Deserialize, Serialize};
use std::io::Write as IoWrite;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{field::Field, field::Visit, Event, Level, Subscriber};
use tracing_subscriber::{
    fmt::{format::Writer, FmtContext, FormatEvent, FormatFields},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter, Layer, Registry,
};

// -----------------------------------------------------------------------------
// 1. Node Log Record Schema
// -----------------------------------------------------------------------------

/// The standard envelope for all node-level structured log records.
/// This is what the JSON formatter emits - one JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLogRecord {
    /// ISO-8601 timestamp with timezone (UTC).
    pub timestamp: String,
    /// Log level: "TRACE", "DEBUG", "INFO", "WARN", "ERROR"
    pub level: String,
    /// The Rust module path that emitted this log.
    pub target: String,
    /// The span name (function or logical operation).
    pub span: Option<String>,
    /// The primary message of the log line.
    pub message: String,
    /// The node ID that emitted this log.
    pub node_id: String,
    /// The app ID this log relates to (if any).
    pub app_id: Option<String>,
    /// The instance ID this log relates to (if any).
    pub instance_id: Option<String>,
    /// OpenTelemetry trace ID for cross-service correlation.
    pub trace_id: Option<String>,
    /// OpenTelemetry span ID.
    pub span_id: Option<String>,
    /// Additional structured key-value pairs from the tracing call.
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Source file path (only in debug builds).
    pub source_file: Option<String>,
    /// Source line number (only in debug builds).
    pub source_line: Option<u32>,
}

// -----------------------------------------------------------------------------
// 2. JSON Formatter Configuration
// -----------------------------------------------------------------------------

/// Configuration for the structured logging subsystem.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    /// Output format: "json" (production) or "text" (development).
    pub format: LogFormat,
    /// Output destination: stdout, stderr, or a file path.
    pub output: LogOutput,
    /// Default log level directive (e.g. "info").
    /// Overridden by `RUST_LOG` environment variable.
    pub default_level: String,
    /// Per-module log level overrides.
    pub module_levels: std::collections::HashMap<String, String>,
    /// Enable log sampling for INFO and below.
    pub sampling_enabled: bool,
    /// Sampling rate for INFO logs (1 = 100 %, 10 = 10 %, 100 = 1 %).
    pub info_sample_rate: u64,
    /// Sampling rate for DEBUG logs.
    pub debug_sample_rate: u64,
    /// Sampling rate for TRACE logs.
    pub trace_sample_rate: u64,
    /// The node ID to include in every log record.
    pub node_id: String,
    /// Include source file and line number in every record.
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

// -----------------------------------------------------------------------------
// 2.1 JSON Event Formatter
// -----------------------------------------------------------------------------

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

        // Collect all fields from the event
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);

        let mut record = serde_json::Map::new();

        // Required fields
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

        // Span name (from the current span context)
        if let Some(span) = ctx.lookup_current() {
            let name = span.name().to_string();
            record.insert("span".to_string(), serde_json::Value::String(name));
        }

        // Node ID
        record.insert(
            "node_id".to_string(),
            serde_json::Value::String(self.node_id.clone()),
        );

        // Message (from the "message" field, or empty)
        let message = visitor
            .fields
            .remove("message")
            .unwrap_or_else(|| serde_json::Value::String(String::new()));
        record.insert("message".to_string(), message);

        // Known correlation fields - extract from visitor.fields
        for key in &["app_id", "instance_id", "trace_id", "span_id"] {
            if let Some(value) = visitor.fields.remove(*key) {
                record.insert(key.to_string(), value);
            }
        }

        // Remaining fields go into the "fields" map
        if !visitor.fields.is_empty() {
            record.insert(
                "fields".to_string(),
                serde_json::Value::Object(visitor.fields),
            );
        }

        // Source location (debug builds)
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

        // Write as a single JSON line
        let json = serde_json::Value::Object(record);
        writeln!(writer, "{}", json)
    }
}

/// Collects fields from a tracing event into a JSON object.
#[derive(Default)]
struct FieldCollector {
    fields: serde_json::Map<String, serde_json::Value>,
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
        // serde_json doesn't support NaN/Inf - convert to strings for safety
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

// -----------------------------------------------------------------------------
// 2.2 Initialization
// -----------------------------------------------------------------------------

/// Writer abstraction that routes to stdout, stderr, or a file.
enum LogWriter {
    Stdout,
    Stderr,
    File(Arc<std::sync::Mutex<std::fs::File>>),
}

fn build_log_writer(output: &LogOutput) -> Result<LogWriter, String> {
    match output {
        LogOutput::Stdout => Ok(LogWriter::Stdout),
        LogOutput::Stderr => Ok(LogWriter::Stderr),
        LogOutput::File { path } => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("failed to open log file {}: {}", path.display(), e))?;
            Ok(LogWriter::File(Arc::new(std::sync::Mutex::new(file))))
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogWriterGuard<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        match self {
            LogWriter::Stdout => LogWriterGuard::Stdout(std::io::stdout()),
            LogWriter::Stderr => LogWriterGuard::Stderr(std::io::stderr()),
            LogWriter::File(file) => LogWriterGuard::File(FileWriterGuard {
                guard: file.lock().unwrap_or_else(|e| e.into_inner()),
            }),
        }
    }
}

enum LogWriterGuard<'a> {
    Stdout(std::io::Stdout),
    Stderr(std::io::Stderr),
    File(FileWriterGuard<'a>),
}

struct FileWriterGuard<'a> {
    guard: std::sync::MutexGuard<'a, std::fs::File>,
}

impl<'a> IoWrite for FileWriterGuard<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.guard.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.guard.flush()
    }
}

impl<'a> IoWrite for LogWriterGuard<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            LogWriterGuard::Stdout(w) => w.write(buf),
            LogWriterGuard::Stderr(w) => w.write(buf),
            LogWriterGuard::File(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            LogWriterGuard::Stdout(w) => w.flush(),
            LogWriterGuard::Stderr(w) => w.flush(),
            LogWriterGuard::File(w) => w.flush(),
        }
    }
}

/// Handle for hot-reloading log levels at runtime.
pub struct LogReloadHandle {
    reload: tracing_subscriber::reload::Handle<EnvFilter, Registry>,
}

impl LogReloadHandle {
    /// Create a handle from an existing reload handle.
    /// This is useful for tests that need to construct a handle without
    /// installing a global subscriber.
    pub fn new(reload: tracing_subscriber::reload::Handle<EnvFilter, Registry>) -> Self {
        Self { reload }
    }
}

impl LogReloadHandle {
    /// Update the log level filter at runtime (no restart required).
    /// Accepts the same format as `RUST_LOG`: `"debug,proxy::service=trace"`
    pub fn update_levels(&self, directives: &str) -> Result<(), String> {
        let new_filter = EnvFilter::new(directives);
        self.reload.reload(new_filter).map_err(|e| e.to_string())
    }

    /// Add or update a per-module log level.
    ///
    /// Composes the new directive with the existing filter rather than replacing it,
    /// so previously set levels are preserved.
    pub fn set_module_level(&self, module: &str, level: &str) -> Result<(), String> {
        let directive = format!("{}={}", module, level);
        let new_filter = self
            .reload
            .with_current(|current| {
                let current_str = current.to_string();
                let combined = if current_str.is_empty() {
                    directive.clone()
                } else {
                    format!("{},{}", current_str, directive)
                };
                EnvFilter::new(combined)
            })
            .map_err(|e| e.to_string())?;
        self.reload.reload(new_filter).map_err(|e| e.to_string())
    }
}

impl Clone for LogReloadHandle {
    fn clone(&self) -> Self {
        LogReloadHandle {
            reload: self.reload.clone(),
        }
    }
}

/// Initialize the structured logging subsystem.
/// Must be called once at program startup, before any `tracing` calls.
pub fn init_logging(config: &LoggingConfig) -> LogReloadHandle {
    // Build the env filter from config + RUST_LOG
    let mut directives = String::new();
    directives.push_str(&config.default_level);

    for (module, level) in &config.module_levels {
        directives.push_str(&format!(",{}={}", module, level));
    }

    // RUST_LOG overrides the config defaults
    let env_filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(&directives)
    };

    // Create the reloadable layer (for hot-reload of log levels)
    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);

    // Writer selection
    let writer = build_log_writer(&config.output).unwrap_or_else(|e| {
        eprintln!("{e}; exiting");
        std::process::exit(1);
    });

    // Create the formatting layer
    match config.format {
        LogFormat::Json => {
            let formatter = NodeJsonFormatter::new(config.node_id.clone(), config.include_source);
            let fmt_layer = tracing_subscriber::fmt::layer()
                .event_format(formatter)
                .with_writer(writer);

            // Optional: sampling layer
            let sampling_layer = if config.sampling_enabled {
                Some(SamplingLayer::new(
                    config.info_sample_rate,
                    config.debug_sample_rate,
                    config.trace_sample_rate,
                ))
            } else {
                None
            };

            let subscriber = Registry::default()
                .with(filter_layer)
                .with(fmt_layer)
                .with(sampling_layer);
            subscriber.init();
        }
        LogFormat::Text => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .with_writer(writer);

            let sampling_layer = if config.sampling_enabled {
                Some(SamplingLayer::new(
                    config.info_sample_rate,
                    config.debug_sample_rate,
                    config.trace_sample_rate,
                ))
            } else {
                None
            };

            let subscriber = Registry::default()
                .with(filter_layer)
                .with(fmt_layer)
                .with(sampling_layer);
            subscriber.init();
        }
    }

    LogReloadHandle {
        reload: reload_handle,
    }
}

// -----------------------------------------------------------------------------
// 3. Log Sampling Layer
// -----------------------------------------------------------------------------

/// A tracing layer that samples logs at INFO, DEBUG, and TRACE levels.
/// WARN and ERROR are always emitted (100 %).
#[derive(Debug)]
pub struct SamplingLayer {
    /// Emit every Nth INFO log (1 = all, 10 = 10 %, 100 = 1 %).
    info_rate: Arc<AtomicU64>,
    /// Emit every Nth DEBUG log.
    debug_rate: Arc<AtomicU64>,
    /// Emit every Nth TRACE log.
    trace_rate: Arc<AtomicU64>,
    /// Counters for each level.
    info_counter: Arc<AtomicU64>,
    debug_counter: Arc<AtomicU64>,
    trace_counter: Arc<AtomicU64>,
}

impl SamplingLayer {
    pub fn new(info_rate: u64, debug_rate: u64, trace_rate: u64) -> Self {
        SamplingLayer {
            info_rate: Arc::new(AtomicU64::new(info_rate.max(1))),
            debug_rate: Arc::new(AtomicU64::new(debug_rate.max(1))),
            trace_rate: Arc::new(AtomicU64::new(trace_rate.max(1))),
            info_counter: Arc::new(AtomicU64::new(0)),
            debug_counter: Arc::new(AtomicU64::new(0)),
            trace_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Update sampling rates at runtime (hot-reload).
    pub fn set_rates(&self, info: u64, debug: u64, trace: u64) {
        self.info_rate.store(info.max(1), Ordering::Relaxed);
        self.debug_rate.store(debug.max(1), Ordering::Relaxed);
        self.trace_rate.store(trace.max(1), Ordering::Relaxed);
    }
}

impl<S: Subscriber> Layer<S> for SamplingLayer {
    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        let level = metadata.level();
        match *level {
            Level::ERROR | Level::WARN => true, // Always emit
            Level::INFO => {
                let count = self.info_counter.fetch_add(1, Ordering::Relaxed);
                count.is_multiple_of(self.info_rate.load(Ordering::Relaxed))
            }
            Level::DEBUG => {
                let count = self.debug_counter.fetch_add(1, Ordering::Relaxed);
                count.is_multiple_of(self.debug_rate.load(Ordering::Relaxed))
            }
            Level::TRACE => {
                let count = self.trace_counter.fetch_add(1, Ordering::Relaxed);
                count.is_multiple_of(self.trace_rate.load(Ordering::Relaxed))
            }
        }
    }
}

// -----------------------------------------------------------------------------
// 4. Audit Log Separation
// -----------------------------------------------------------------------------

/// An audit log record for security-sensitive operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogRecord {
    /// ISO-8601 timestamp.
    pub timestamp: String,
    /// Always "audit" - distinguishes from operational logs.
    pub log_type: String,
    /// The action that was performed.
    pub action: String,
    /// Who performed the action.
    pub actor: String,
    /// The node that processed the action.
    pub node_id: String,
    /// The app affected (if applicable).
    pub app_id: Option<String>,
    /// Whether the action succeeded.
    pub success: bool,
    /// Error message if the action failed.
    pub error: Option<String>,
    /// Source IP of the request (for admin API actions).
    pub source_ip: Option<String>,
    /// Additional context specific to the action.
    pub details: serde_json::Map<String, serde_json::Value>,
}

/// Output destination for audit logs.
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
/// Writes to a separate file (or separate NATS subject) from operational logs.
#[derive(Debug, Clone)]
pub struct AuditLogger {
    node_id: String,
    tx: tokio::sync::mpsc::Sender<AuditLogRecord>,
    dropped_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl AuditLogger {
    /// Start the audit logger with the specified output.
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

    /// Set the node_id after startup (once configuration is loaded).
    pub fn set_node_id(&mut self, node_id: String) {
        self.node_id = node_id;
    }

    /// Record an audit event.
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
            use std::sync::atomic::Ordering;
            let dropped = self.dropped_count.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_multiple_of(1000) {
                tracing::warn!("audit log record dropped ({} total dropped)", dropped);
            }
        }
    }

    /// Record an audit event with full details.
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
            use std::sync::atomic::Ordering;
            let dropped = self.dropped_count.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_multiple_of(1000) {
                tracing::warn!("audit log record dropped ({} total dropped)", dropped);
            }
        }
    }
}

// -----------------------------------------------------------------------------
// 5. Log Forwarding Configuration
// -----------------------------------------------------------------------------

/// Configuration for the log forwarder.
#[derive(Debug, Clone)]
pub struct LogForwarderConfig {
    /// Enabled forwarder sinks.
    pub sinks: Vec<ForwarderSinkConfig>,
    /// Channel buffer capacity (backpressure).
    pub buffer_capacity: usize,
    /// Batch size before flushing.
    pub batch_size: usize,
    /// Flush interval in milliseconds.
    pub flush_interval_ms: u64,
}

#[derive(Debug, Clone)]
pub enum ForwarderSinkConfig {
    Loki {
        endpoint: String,
        labels: Vec<(String, String)>,
    },
    Elasticsearch {
        endpoint: String,
        index_prefix: String,
    },
    Vector {
        endpoint: String,
    },
    Http {
        endpoint: String,
    },
    Nats {
        subject: String,
    },
}

impl LogForwarderConfig {
    pub fn flush_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.flush_interval_ms)
    }
}

// -----------------------------------------------------------------------------
// 7. Log Rotation & Retention
// -----------------------------------------------------------------------------

/// Configuration for log file rotation.
#[derive(Debug, Clone)]
pub struct LogRotationConfig {
    /// Maximum size of a single log file before rotation.
    pub max_file_size_bytes: u64,
    /// Maximum number of rotated log files to keep.
    pub max_files: u32,
    /// Maximum age of a log file before rotation.
    pub max_age: std::time::Duration,
    /// Compress rotated files with gzip.
    pub compress: bool,
}

impl Default for LogRotationConfig {
    fn default() -> Self {
        LogRotationConfig {
            max_file_size_bytes: 100 * 1024 * 1024, // 100 MB
            max_files: 10,
            max_age: std::time::Duration::from_secs(24 * 3600),
            compress: true,
        }
    }
}

/// A file writer that rotates when the file exceeds the configured size.
struct RotatingFileState {
    current_size: u64,
    current_file: Option<std::fs::File>,
}

pub struct RotatingFileWriter {
    path: std::path::PathBuf,
    config: LogRotationConfig,
    state: std::sync::Mutex<RotatingFileState>,
}

impl RotatingFileWriter {
    pub fn new(path: std::path::PathBuf, config: LogRotationConfig) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        let current_size = file.metadata()?.len();

        Ok(RotatingFileWriter {
            path,
            config,
            state: std::sync::Mutex::new(RotatingFileState {
                current_size,
                current_file: Some(file),
            }),
        })
    }

    /// Write a line to the current log file, rotating if necessary.
    pub fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.current_size > self.config.max_file_size_bytes {
            self.rotate_with_state(&mut state)?;
        }

        if let Some(ref mut file) = state.current_file {
            let bytes = line.as_bytes();
            file.write_all(bytes)?;
            file.write_all(b"\n")?;
            state.current_size += bytes.len() as u64 + 1;
        }

        Ok(())
    }

    /// Rotate the current log file.
    fn rotate_with_state(
        &self,
        state: &mut std::sync::MutexGuard<'_, RotatingFileState>,
    ) -> std::io::Result<()> {
        // Close the current file
        state.current_file = None;

        // Remove the oldest rotated file if we've hit the limit
        let oldest = format!("{}.{}", self.path.display(), self.config.max_files);
        let _ = std::fs::remove_file(&oldest);

        // Shift rotated files: .N -> .N+1
        for i in (1..self.config.max_files).rev() {
            let from = format!("{}.{}", self.path.display(), i);
            let to = format!("{}.{}", self.path.display(), i + 1);
            let _ = std::fs::rename(&from, &to);
        }

        // Move current file to .1
        let rotated = format!("{}.1", self.path.display());
        let _ = std::fs::rename(&self.path, &rotated);

        // Optionally compress the rotated file
        if self.config.compress {
            self.compress_file(&rotated);
        }

        // Open a new current file
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        state.current_file = Some(file);
        state.current_size = 0;

        Ok(())
    }

    /// Compress a rotated log file with gzip.
    fn compress_file(&self, path: &str) {
        let gz_path = format!("{}.gz", path);
        let mut input = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("failed to open log file for compression '{}': {}", path, e);
                return;
            }
        };
        let mut output = match std::fs::File::create(&gz_path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("failed to create compressed file '{}': {}", gz_path, e);
                return;
            }
        };
        let mut encoder = flate2::write::GzEncoder::new(&mut output, flate2::Compression::fast());
        if let Err(e) = std::io::copy(&mut input, &mut encoder) {
            tracing::warn!("failed to compress log file '{}': {}", path, e);
            return;
        }
        if let Err(e) = encoder.finish() {
            tracing::warn!("failed to finish gzip encoding for '{}': {}", gz_path, e);
        }
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!("failed to remove uncompressed log file '{}': {}", path, e);
        }
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
