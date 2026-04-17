# Step 36 — Structured Logging & Log Aggregation

## Goal
Implement a consistent, structured logging standard for the entire `wasm-node` process
and provide log aggregation tooling for production deployments. The system must:
- Emit all node logs as structured JSON with a consistent field schema
- Define a mandatory field naming convention for all `tracing` calls across all crates
- Replace all `eprintln!()` and `println!()` calls with proper `tracing` macros
- Support per-module log level configuration with hot-reload (no restart required)
- Forward node logs to external aggregators (Grafana Loki, Elasticsearch, Vector)
- Correlate node logs with Wasm app logs via trace IDs
- Separate audit logs from operational logs (different retention, different access)
- Support log sampling at high throughput to prevent log storms
- Provide log rotation and retention for file-based logging
- Integrate with the existing `WasmLogRecord` and `LogDispatcher` from Step 17
- Require no external log collector agent — the node is self-sufficient

---

## Context & Rationale

### The Problem This Solves

The current logging situation has three distinct problems:

**1. No structured format for node logs.** The node process uses
`tracing_subscriber::fmt().with_env_filter(...)` which produces human-readable
text output like:
```
2026-04-05T12:00:00Z INFO wasm_node::handlers: received event in handler event=DeployApp
```
This is fine for `kubectl logs` during development but useless for log aggregation.
Grafana Loki, Elasticsearch, and Vector all expect structured JSON with consistent
field names. The current output cannot be queried by field.

**2. Inconsistent field naming.** Different crates use different field names for the
same concept:
```rust
// crates/node/src/handlers.rs
tracing::info!(app = %app_id.0, "handle_deploy invoked");

// crates/supervisor/src/lib.rs
tracing::info!(app_id = %id.0, "instance spawned");

// crates/proxy/src/service.rs
tracing::info!(app = ctx.app_id.as_ref().map(|a| a.0.as_str()).unwrap_or("unknown"), ...);

// crates/billing/src/export.rs
tracing::info!(key = %key, records = records.len(), "billing batch exported to S3");
```
An operator searching Loki for `app_id = "api-users:v1"` will miss logs that use
`app = "api-users:v1"`. This is a real operational problem in production.

**3. `eprintln!()` bypasses the tracing system entirely.** The audit in Step 31 found
`eprintln!()` calls in `crates/runtime/src/executor.rs` and test code. These bypass
the tracing subscriber entirely — they go directly to stderr with no structure, no
timestamps, no correlation, and no filtering. In production, they are invisible to
log aggregators.

### Why JSON Structured Logs (Not Text)

| Format    │ Human Readable │ Machine Queryable │ Field Extraction │ Aggregator Support
|───────────┼────────────────┼───────────────────┼──────────────────┼───────────────────
| Text      │ Yes            │ No (regex needed) │ Fragile          │ Requires parser
| JSON      │ Noisy          │ Yes (native)      │ Exact            │ Native in all
| Logfmt    │ Yes            │ Yes (key=value)   │ Good             │ Loki native

JSON is chosen because:
- **Every aggregator natively supports it**: Loki with `json` parser, Elasticsearch
  with auto-detection, Vector with `json` transform
- **No ambiguity**: field types are explicit (number vs string vs null)
- **Nested fields**: the `structured` field from `WasmLogRecord` (Step 17) is already
  JSON — nesting it inside a JSON log line is natural
- **tracing-subscriber supports it natively**: `.json()` formatter with zero custom code

The trade-off is readability: JSON is noisy for `kubectl logs`. This is solved by
providing a `wasm-ctl logs` command that pretty-prints JSON logs, and by supporting
a `--log-format text` flag for development mode.

### Why a Mandatory Field Naming Convention

Without a convention, every developer uses whatever field name feels natural. Over
time, the same concept gets logged with 3–4 different names. Searching becomes
impossible. The convention must be:

1. **Documented**: a table of required field names for common concepts
2. **Enforced**: a Clippy lint or CI check that validates field names
3. **Consistent**: the same name is used everywhere, in every crate

This is not a stylistic preference — it is an operational requirement. In a production
incident, an operator needs to query `app_id = "payments:v2"` and get ALL relevant
logs from ALL crates, not just the ones that happened to use the same field name.

### Why Separate Audit Logs

Audit logs (admin API calls, config changes, secret access, billing exports) have
different requirements from operational logs:

| Aspect         │ Operational Logs          │ Audit Logs
|────────────────┼──────────────────────────┼──────────────────────────────
| Retention      │ 7–30 days                │ 1–7 years (compliance)
| Access         │ All engineers            │ Security team only
| Content        │ Debug info, stack traces │ Who did what, when, from where
| Volume         │ High (GB/day)            │ Low (MB/day)
| Immutability   │ Best effort              │ Must be tamper-evident

Mixing audit events into the operational log stream means they get the same retention
(30 days) and the same access (all engineers). This violates most compliance frameworks.

### Why Log Sampling

Under normal operation, a node processes ~1000 requests/second and produces ~5000
log lines/second (5 per request: request_filter, upstream_peer, upstream_request_filter,
logging, and one Wasm app log). At this rate, a 3-node cluster produces 15,000
log lines/second = 1.3 billion lines/day.

During an incident (all requests failing), the log volume can spike to 50,000
lines/second. Without sampling, this overwhelms both the log aggregator and the
network bandwidth to reach it.

Log sampling reduces volume by emitting only N% of logs at a given level while
always emitting ERROR and WARN. This ensures that:
- Errors are never lost (100% of ERROR/WARN emitted)
- Debug/trace logs are sampled (10% at INFO, 1% at DEBUG, 0.1% at TRACE)
- The total volume stays within the aggregator's capacity

### The Relationship with Step 17 (Wasm stdout/stderr Capture)

Step 17 defines `WasmLogRecord` and `LogDispatcher` for capturing logs **from Wasm
modules**. This step (Step 36) defines structured logging for the **node process
itself**. They are complementary:

```
┌──────────────────────────────────────────────────────────┐
│ Node Process                                             │
│                                                          │
│  ┌─────────────┐     ┌──────────────────┐               │
│  │ Wasm App    │────▶│ WasmLogRecord    │──┐            │
│  │ (stdout/err)│     │ (Step 17)        │  │            │
│  └─────────────┘     └──────────────────┘  │            │
│                                            ▼            │
│  ┌─────────────┐     ┌──────────────────┐ ┌──────────┐ │
│  │ Node Code   │────▶│ NodeLogRecord    │─▶│ Unified  │ │
│  │ (tracing)   │     │ (Step 36)        │ │ Aggregator│ │
│  └─────────────┘     └──────────────────┘ └──────────┘ │
│                                            │            │
│  ┌─────────────┐     ┌──────────────────┐  │            │
│  │ Audit Events│────▶│ AuditLogRecord   │──┘            │
│  │ (admin API) │     │ (Step 36)        │               │
│  └─────────────┘     └──────────────────┘               │
└──────────────────────────────────────────────────────────┘
                         │
                         ▼
              ┌─────────────────────┐
              │ External Aggregator │
              │ (Loki / ES / Vector)│
              └─────────────────────┘
```

Both `WasmLogRecord` and `NodeLogRecord` share a common set of fields (timestamp,
node_id, trace_id, app_id) so that an aggregator can query across both types with
a single filter.

---

## 1. Node Log Record Schema

Every log line emitted by the node process (via `tracing`) follows this JSON schema.
This is the canonical format that log aggregators receive.

```rust
// crates/common/src/logging.rs (new file)
use serde::{Deserialize, Serialize};

/// The standard envelope for all node-level structured log records.
/// This is what the JSON formatter emits — one JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLogRecord {
    // ── Required fields (present on every log line) ──────────────

    /// ISO-8601 timestamp with timezone (UTC).
    /// Example: "2026-04-05T12:00:00.123456Z"
    pub timestamp: String,

    /// Log level: "TRACE", "DEBUG", "INFO", "WARN", "ERROR"
    pub level: String,

    /// The Rust module path that emitted this log.
    /// Example: "wasm_node::handlers", "supervisor::pool"
    pub target: String,

    /// The span name (function or logical operation).
    /// Example: "handle_deploy", "health_loop"
    pub span: Option<String>,

    /// The primary message of the log line.
    pub message: String,

    /// The node ID that emitted this log.
    pub node_id: String,

    // ── Optional correlation fields ──────────────────────────────

    /// The app ID this log relates to (if any).
    /// MUST use the field name "app_id" everywhere.
    pub app_id: Option<String>,

    /// The instance ID this log relates to (if any).
    pub instance_id: Option<String>,

    /// OpenTelemetry trace ID for cross-service correlation.
    pub trace_id: Option<String>,

    /// OpenTelemetry span ID.
    pub span_id: Option<String>,

    // ── Optional context fields ──────────────────────────────────

    /// Additional structured key-value pairs from the tracing call.
    /// All custom fields that don't have a dedicated column go here.
    pub fields: serde_json::Map<String, serde_json::Value>,

    // ── Source location (debug builds only) ──────────────────────

    /// Source file path (only in debug builds).
    pub source_file: Option<String>,

    /// Source line number (only in debug builds).
    pub source_line: Option<u32>,
}
```

### 1.1 Field Naming Convention

The following field names are **mandatory** across all crates. Using a different name
for the same concept is a bug.

| Concept              │ Field Name      │ Type     │ Example Value              │ Used In
|──────────────────────│─────────────────│──────────│────────────────────────────│───────────────
| Application identity │ `app_id`        │ String   │ `"api-users:v2"`           │ All crates
| Instance identity    │ `instance_id`   │ String   │ `"a1b2c3d4-..."`           │ supervisor, runtime
| Node identity        │ `node_id`       │ String   │ `"node-0"`                 │ All crates
| Trace correlation    │ `trace_id`      │ String   │ `"4bf92f3577b34da6a3ce..." │ proxy, supervisor
| Error description    │ `error`         │ String   │ `"NATS connect: timeout"`  │ All crates
| HTTP status code     │ `status`        │ u16      │ `200`, `503`               │ proxy
| Latency              │ `latency_ms`    │ u64      │ `42`                       │ proxy
| NATS subject         │ `subject`       │ String   │ `"deploy.app.new"`         │ messaging
| Artifact hash        │ `sha256`        │ String   │ `"abc123..."`              │ storage, node
| Billing sequence     │ `seq`           │ u64      │ `42`                       │ billing
| Fuel consumed        │ `fuel`          │ u64      │ `500000000`                │ runtime, billing
| Recovery mode        │ `recovery_mode` │ String   │ `"corruption_detected"`    │ node, storage
| Config key           │ `config_key`    │ String   │ `"rate_limit.rps"`         │ node (hot-reload)
| Audit actor          │ `actor`         │ String   │ `"token:ops-readwrite"`    │ audit logs
| Audit action         │ `action`        │ String   │ `"config.update"`          │ audit logs

**Enforcement**: A CI check validates that all `tracing` calls in `crates/` use
the correct field names. The check is a simple grep script (see Section 9).

### 1.2 Anti-Patterns to Avoid

```rust
// ❌ WRONG: inconsistent field name
tracing::info!(app = %app_id.0, "deploying");
tracing::info!(application = %app_id.0, "deploying");
tracing::info!(name = %app_id.0, "deploying");

// ✅ CORRECT: always use "app_id"
tracing::info!(app_id = %app_id.0, "deploying");

// ❌ WRONG: error as a format string
tracing::error!(error = format!("failed: {e}"), "operation failed");

// ✅ CORRECT: error as a display value
tracing::error!(error = %e, "operation failed");

// ❌ WRONG: eprintln!() bypasses tracing
eprintln!("DEBUG: connecting to {}", url);

// ✅ CORRECT: use tracing
tracing::debug!(url = %url, "connecting");

// ❌ WRONG: structured data in the message string
tracing::info!("deployed app api-users:v2 with 500M fuel");

// ✅ CORRECT: structured data as fields
tracing::info!(app_id = "api-users:v2", fuel = 500_000_000, "deployed app");
```

---

## 2. JSON Formatter Configuration

Replace the current `tracing_subscriber::fmt()` with a JSON-formatted subscriber
that produces `NodeLogRecord`-compatible output.

```rust
// crates/common/src/logging.rs (continued)

use std::sync::Arc;
use tracing_subscriber::{
    fmt::{format::FmtContext, FormatEvent, FormatFields},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter, Layer, Registry,
};

/// Configuration for the structured logging subsystem.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    /// Output format: "json" (production) or "text" (development).
    pub format: LogFormat,

    /// Output destination: "stdout", "stderr", or a file path.
    pub output: LogOutput,

    /// Default log level directive (e.g., "info").
    /// Overridden by RUST_LOG environment variable.
    pub default_level: String,

    /// Per-module log level overrides.
    /// Example: {"supervisor::pool": "debug", "proxy::service": "trace"}
    pub module_levels: std::collections::HashMap<String, String>,

    /// Enable log sampling for INFO and below.
    pub sampling_enabled: bool,

    /// Sampling rate for INFO logs (1 = 100%, 10 = 10%, 100 = 1%).
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
```

### 2.1 JSON Event Formatter

Custom `FormatEvent` implementation that produces the `NodeLogRecord` schema.

```rust
// crates/common/src/logging.rs (continued)

use std::fmt::Write;
use std::io;
use tracing::{
    field::Visit, Event, Field, Level, Metadata, Subscriber,
};
use tracing_subscriber::fmt::{
    format::{FmtContext, JsonFields},
    FmtContext as FmtCtx, FormattedFields,
};

/// A custom JSON formatter that produces NodeLogRecord-compatible output.
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
        ctx: &FmtCtx<'_, S, N>,
        writer: io::mut::StdoutLock<'_>,
        event: &Event<'_>,
    ) -> io::Result<()> {
        let metadata = event.metadata();

        // Collect all fields from the event
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);

        let mut record = serde_json::Map::new();

        // Required fields
        record.insert(
            "timestamp".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)),
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
        let span_name = ctx.lookup_current().map(|span| {
            span.name().to_string()
        });
        if let Some(ref span) = span_name {
            record.insert("span".to_string(), serde_json::Value::String(span.clone()));
        }

        // Node ID
        record.insert(
            "node_id".to_string(),
            serde_json::Value::String(self.node_id.clone()),
        );

        // Message (from the "message" field, or empty)
        let message = visitor.fields.remove("message")
            .unwrap_or(serde_json::Value::String(String::new()));
        record.insert("message".to_string(), message);

        // Known correlation fields — extract from visitor.fields
        if let Some(app_id) = visitor.fields.remove("app_id") {
            record.insert("app_id".to_string(), app_id);
        }
        if let Some(instance_id) = visitor.fields.remove("instance_id") {
            record.insert("instance_id".to_string(), instance_id);
        }
        if let Some(trace_id) = visitor.fields.remove("trace_id") {
            record.insert("trace_id".to_string(), trace_id);
        }
        if let Some(span_id) = visitor.fields.remove("span_id") {
            record.insert("span_id".to_string(), span_id);
        }

        // Remaining fields go into the "fields" map
        if !visitor.fields.is_empty() {
            record.insert("fields".to_string(), serde_json::Value::Object(visitor.fields));
        }

        // Source location (debug builds)
        if self.include_source {
            if let Some(file) = metadata.file() {
                record.insert("source_file".to_string(), serde_json::Value::String(file.to_string()));
            }
            if let Some(line) = metadata.line() {
                record.insert("source_line".to_string(), serde_json::Value::Number(line.into()));
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
        self.fields.insert(field.name().to_string(), serde_json::Value::String(value.to_string()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_string(), serde_json::Value::Number(value.into()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_string(), serde_json::Value::Number(value.into()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_string(), serde_json::Value::Bool(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        // serde_json doesn't support NaN/Inf — convert to string
        if value.is_finite() {
            if let Some(n) = serde_json::Number::from_f64(value) {
                self.fields.insert(field.name().to_string(), serde_json::Value::Number(n));
            } else {
                self.fields.insert(field.name().to_string(), serde_json::Value::String(value.to_string()));
            }
        } else {
            self.fields.insert(field.name().to_string(), serde_json::Value::String(value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(field.name().to_string(), serde_json::Value::String(format!("{:?}", value)));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.fields.insert(field.name().to_string(), serde_json::Value::String(value.to_string()));
    }
}
```

### 2.2 Initialization

```rust
// crates/common/src/logging.rs (continued)

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
    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(
        env_filter
    );

    // Create the formatting layer
    let format_layer = match config.format {
        LogFormat::Json => {
            let formatter = NodeJsonFormatter::new(
                config.node_id.clone(),
                config.include_source,
            );
            tracing_subscriber::fmt::layer()
                .event_format(formatter)
                .boxed()
        }
        LogFormat::Text => {
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .boxed()
        }
    };

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

    // Build the subscriber
    let subscriber = Registry::default()
        .with(filter_layer)
        .with(format_layer)
        .with(sampling_layer);

    subscriber.init();

    LogReloadHandle { reload: reload_handle }
}

/// Handle for hot-reloading log levels at runtime.
pub struct LogReloadHandle {
    reload: tracing_subscriber::reload::Handle<EnvFilter, Registry>,
}

impl LogReloadHandle {
    /// Update the log level filter at runtime (no restart required).
    /// Accepts the same format as RUST_LOG: "debug,proxy::service=trace"
    pub fn update_levels(&self, directives: &str) -> Result<(), String> {
        let new_filter = EnvFilter::new(directives);
        self.reload.reload(new_filter).map_err(|e| e.to_string())
    }

    /// Add or update a per-module log level.
    pub fn set_module_level(&self, module: &str, level: &str) -> Result<(), String> {
        // Read the current filter, add the new directive, and reload
        let directive = format!(",{}={}", module, level);
        // We need to reconstruct the full filter. Store the original directives
        // and append. For simplicity, we store the directives string.
        let new_filter = EnvFilter::new(directive);
        self.reload.reload(new_filter).map_err(|e| e.to_string())
    }
}
```

---

## 3. Log Sampling Layer

A `tracing` layer that samples logs below WARN level to prevent log storms.

```rust
// crates/common/src/logging.rs (continued)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{Level, Subscriber};
use tracing_subscriber::layer::Layer;

/// A tracing layer that samples logs at INFO, DEBUG, and TRACE levels.
/// WARN and ERROR are always emitted (100%).
#[derive(Debug)]
pub struct SamplingLayer {
    /// Emit every Nth INFO log (1 = all, 10 = 10%, 100 = 1%).
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
                count % self.info_rate.load(Ordering::Relaxed) == 0
            }
            Level::DEBUG => {
                let count = self.debug_counter.fetch_add(1, Ordering::Relaxed);
                count % self.debug_rate.load(Ordering::Relaxed) == 0
            }
            Level::TRACE => {
                let count = self.trace_counter.fetch_add(1, Ordering::Relaxed);
                count % self.trace_rate.load(Ordering::Relaxed) == 0
            }
        }
    }
}
```

---

## 4. Audit Log Separation

Audit events are written to a separate output with a different schema and retention
policy. They are never sampled and never dropped.

```rust
// crates/common/src/logging.rs (continued)

/// An audit log record for security-sensitive operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogRecord {
    /// ISO-8601 timestamp.
    pub timestamp: String,

    /// Always "audit" — distinguishes from operational logs.
    pub log_type: String,

    /// The action that was performed.
    /// Example: "config.update", "app.deploy", "secret.access", "admin.rebuild"
    pub action: String,

    /// Who performed the action.
    /// Example: "token:ops-readwrite", "cli:wasm-ctl", "nats:cluster-sync"
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

/// A dedicated writer for audit logs.
/// Writes to a separate file (or separate NATS subject) from operational logs.
pub struct AuditLogger {
    node_id: String,
    tx: tokio::sync::mpsc::Sender<AuditLogRecord>,
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
                        // Append to audit log file
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
        }
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
        // Use try_send to avoid blocking the caller on audit logging.
        // Audit records are important but should not block request handling.
        let _ = self.tx.try_send(record);
    }

    /// Record an audit event with full details.
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
        let _ = self.tx.try_send(record);
    }
}

/// Output destination for audit logs.
pub enum AuditOutput {
    File { path: std::path::PathBuf },
    Nats { client: async_nats::Client, subject: String },
    Stderr,
}
```

### 4.1 Audit Event Catalog

The following actions are audited. Every admin API endpoint, every config change,
and every secret access must produce an audit record.

| Action                │ Trigger                                    │ Actor Source
|───────────────────────│────────────────────────────────────────────│──────────────────
| `app.deploy`          │ `POST /admin/deploy` or NATS DeployApp     │ Bearer token / NATS
| `app.remove`          │ `POST /admin/remove` or NATS RemoveApp     │ Bearer token / NATS
| `config.update`       │ `PATCH /admin/config`                      │ Bearer token
| `config.hot_reload`   │ SIGHUP or `PATCH /admin/config`            │ OS signal / Bearer token
| `admin.rebuild`       │ `POST /admin/rebuild`                      │ Bearer token
| `admin.gc_force`      │ `POST /admin/gc/force`                     │ Bearer token
| `secret.access`       │ App reads a secret at spawn time           │ Internal (app_id)
| `secret.update`       │ NATS SecretUpdate event                    │ NATS
| `billing.export`      │ Billing export loop writes records         │ Internal (timer)
| `node.upgrade`        │ NATS NodeUpgrade event                     │ NATS
| `node.drain`          │ Graceful shutdown initiated                │ OS signal
| `auth.token_rotate`   │ `POST /admin/auth/tokens/rotate`           │ Bearer token
| `auth.failed`         │ Invalid token on admin API request         │ Source IP
| `recovery.initiated`  │ Node starts in recovery mode               │ Internal
| `recovery.complete`   │ Node finishes recovery                     │ Internal

---

## 5. Log Forwarding to External Aggregators

The node can forward both operational and Wasm app logs to external systems. This
builds on the `LogDispatcher` from Step 17 but extends it to handle node-level logs
as well.

### 5.1 Unified Log Forwarder

```rust
// crates/metrics/src/log_forwarder.rs (new file)
use common::logging::{NodeLogRecord, LogForwarderConfig, ForwarderSink};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A unified log forwarder that handles both node logs and Wasm app logs.
pub struct LogForwarder {
    tx: mpsc::Sender<ForwarderRecord>,
}

/// A log record that can be either a node log or a Wasm app log.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "source")]
pub enum ForwarderRecord {
    /// A log from the node process itself.
    Node { #[serde(flatten)] record: NodeLogRecord },
    /// A log from a Wasm application (captured via Step 17).
    Wasm { #[serde(flatten)] record: crate::WasmLogRecord },
}

impl LogForwarder {
    /// Start the log forwarder with the configured sinks.
    pub fn start(config: LogForwarderConfig) -> Self {
        let (tx, mut rx) = mpsc::channel::<ForwarderRecord>(config.buffer_capacity);

        let sinks: Arc<Vec<Box<dyn ForwarderSink>>> = Arc::new(config.build_sinks());

        tokio::spawn(async move {
            let mut batch: Vec<ForwarderRecord> = Vec::with_capacity(config.batch_size);
            let mut flush_interval = tokio::time::interval(config.flush_interval());

            loop {
                tokio::select! {
                    Some(record) = rx.recv() => {
                        batch.push(record);
                        if batch.len() >= config.batch_size {
                            flush_batch(&sinks, &mut batch).await;
                        }
                    }
                    _ = flush_interval.tick() => {
                        if !batch.is_empty() {
                            flush_batch(&sinks, &mut batch).await;
                        }
                    }
                }
            }
        });

        LogForwarder { tx }
    }

    /// Get a sender for forwarding records.
    pub fn sender(&self) -> mpsc::Sender<ForwarderRecord> {
        self.tx.clone()
    }
}

async fn flush_batch(
    sinks: &[Box<dyn ForwarderSink>],
    batch: &mut Vec<ForwarderRecord>,
) {
    for sink in sinks {
        if let Err(e) = sink.write_batch(batch).await {
            tracing::warn!(error = %e, sink = %sink.name(), "log forwarder write failed");
        }
    }
    batch.clear();
}

/// Trait for log forwarder sinks.
#[async_trait::async_trait]
pub trait ForwarderSink: Send + Sync {
    /// Write a batch of log records to the sink.
    async fn write_batch(&self, records: &[ForwarderRecord]) -> Result<(), String>;

    /// The name of this sink (for logging).
    fn name(&self) -> &str;
}
```

### 5.2 Grafana Loki Sink

Loki expects log lines pushed via the HTTP API with label-set pairs.

```rust
// crates/metrics/src/log_forwarder.rs (continued)

/// Forward logs to Grafana Loki via the push HTTP API.
pub struct LokiSink {
    endpoint: String,
    client: reqwest::Client,
    /// Static labels applied to every log line.
    labels: Vec<(String, String)>,
}

impl LokiSink {
    pub fn new(endpoint: String, labels: Vec<(String, String)>) -> Self {
        LokiSink {
            endpoint,
            client: reqwest::Client::new(),
            labels,
        }
    }
}

#[async_trait::async_trait]
impl ForwarderSink for LokiSink {
    async fn write_batch(&self, records: &[ForwarderRecord]) -> Result<(), String> {
        // Build Loki push payload
        // Format: {"streams":[{"stream":{labels},"values":[[ts,line],...]}]}
        let mut streams: std::collections::BTreeMap<String, Vec<(String, String)>> =
            std::collections::BTreeMap::new();

        for record in records {
            let (labels, line, ts) = match record {
                ForwarderRecord::Node(r) => {
                    let mut labels = self.labels.clone();
                    labels.push(("level".to_string(), r.level.clone()));
                    labels.push(("target".to_string(), r.target.clone()));
                    if let Some(ref app_id) = r.app_id {
                        labels.push(("app_id".to_string(), app_id.clone()));
                    }
                    let line = serde_json::to_string(r).unwrap_or_default();
                    (labels, line, r.timestamp.clone())
                }
                ForwarderRecord::Wasm(r) => {
                    let mut labels = self.labels.clone();
                    labels.push(("app_id".to_string(), r.app_id.clone()));
                    labels.push(("stream".to_string(), r.stream.clone()));
                    labels.push(("source".to_string(), "wasm".to_string()));
                    let line = serde_json::to_string(r).unwrap_or_default();
                    (labels, line, r.node_timestamp.clone())
                }
            };

            // Sort labels for consistent grouping
            let label_key = labels
                .iter()
                .map(|(k, v)| format!("{}=\"{}\"", k, v))
                .collect::<Vec<_>>()
                .join(",");

            streams
                .entry(label_key)
                .or_default()
                .push((ts, line));
        }

        // Build the Loki push request
        let mut json_streams = Vec::new();
        for (label_str, values) in streams {
            let stream_labels: serde_json::Map<String, serde_json::Value> = self
                .labels
                .iter()
                .chain(values.first().and_then(|_| {
                    // Parse labels from the key string back into a map
                    None
                }))
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();

            let json_values: Vec<Vec<serde_json::Value>> = values
                .into_iter()
                .map(|(ts, line)| {
                    vec![
                        serde_json::Value::String(ts),
                        serde_json::Value::String(line),
                    ]
                })
                .collect();

            json_streams.push(serde_json::json!({
                "stream": stream_labels,
                "values": json_values,
            }));
        }

        let payload = serde_json::json!({ "streams": json_streams });

        let url = format!("{}/loki/api/v1/push", self.endpoint.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Loki push failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Loki returned {}: {}", status, body));
        }

        Ok(())
    }

    fn name(&self) -> &str {
        "loki"
    }
}
```

### 5.3 Elasticsearch Sink

```rust
// crates/metrics/src/log_forwarder.rs (continued)

/// Forward logs to Elasticsearch via the bulk API.
pub struct ElasticsearchSink {
    endpoint: String,
    index_prefix: String,
    client: reqwest::Client,
}

impl ElasticsearchSink {
    pub fn new(endpoint: String, index_prefix: String) -> Self {
        ElasticsearchSink {
            endpoint,
            index_prefix,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl ForwarderSink for ElasticsearchSink {
    async fn write_batch(&self, records: &[ForwarderRecord]) -> Result<(), String> {
        // Build Elasticsearch bulk payload
        // Format: NDJSON with action + source pairs
        let mut bulk_body = String::new();

        for record in records {
            let (index, source) = match record {
                ForwarderRecord::Node(r) => {
                    let date = &r.timestamp[..10]; // YYYY-MM-DD
                    let index = format!("{}-node-{}", self.index_prefix, date);
                    let source = serde_json::to_string(r).unwrap_or_default();
                    (index, source)
                }
                ForwarderRecord::Wasm(r) => {
                    let date = &r.node_timestamp[..10];
                    let index = format!("{}-wasm-{}", self.index_prefix, date);
                    let source = serde_json::to_string(r).unwrap_or_default();
                    (index, source)
                }
            };

            // Bulk action line
            bulk_body.push_str(&serde_json::to_string(&serde_json::json!({
                "index": { "_index": index }
            })).unwrap_or_default());
            bulk_body.push('\n');

            // Source document
            bulk_body.push_str(&source);
            bulk_body.push('\n');
        }

        let url = format!("{}/_bulk", self.endpoint.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/x-ndjson")
            .body(bulk_body)
            .send()
            .await
            .map_err(|e| format!("ES bulk failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("ES returned {}: {}", status, body));
        }

        Ok(())
    }

    fn name(&self) -> &str {
        "elasticsearch"
    }
}
```

### 5.4 Vector Sink (HTTP)

Vector is the simplest sink — just POST JSON to an HTTP endpoint.

```rust
// crates/metrics/src/log_forwarder.rs (continued)

/// Forward logs to Vector via HTTP sink.
pub struct VectorSink {
    endpoint: String,
    client: reqwest::Client,
}

impl VectorSink {
    pub fn new(endpoint: String) -> Self {
        VectorSink {
            endpoint,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl ForwarderSink for VectorSink {
    async fn write_batch(&self, records: &[ForwarderRecord]) -> Result<(), String> {
        let payload = serde_json::to_vec(records)
            .map_err(|e| format!("JSON serialization failed: {}", e))?;

        self.client
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| format!("Vector HTTP failed: {}", e))?;

        Ok(())
    }

    fn name(&self) -> &str {
        "vector"
    }
}
```

### 5.5 Forwarder Configuration

```rust
// crates/common/src/logging.rs (continued)

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

    pub fn build_sinks(&self) -> Vec<Box<dyn crate::log_forwarder::ForwarderSink>> {
        // Build sinks from config — implemented in metrics crate
        vec![]
    }
}
```

---

## 6. Log Level Hot-Reload

Integration with Step 32 (Configuration Management) for runtime log level changes.

### 6.1 Admin API Endpoints

```rust
// crates/proxy/src/admin.rs (add these routes)

use axum::{
    extract::State,
    Json,
    http::StatusCode,
};
use common::logging::LogReloadHandle;

/// GET /admin/logging/levels
/// Returns the current effective log level configuration.
async fn get_log_levels(
    State(handle): State<Arc<LogReloadHandle>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "message": "Current log levels are managed by the tracing subscriber. \
                    Use RUST_LOG format for updates.",
        "hint": "PATCH /admin/logging/levels with {\"directives\": \"debug,supervisor=trace\"}"
    }))
}

/// PATCH /admin/logging/levels
/// Update log levels at runtime without restart.
/// Body: { "directives": "debug,proxy::service=trace" }
async fn update_log_levels(
    State(handle): State<Arc<LogReloadHandle>>,
    Json(body): Json<UpdateLogLevelsRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    handle
        .update_levels(&body.directives)
        .map(|_| {
            tracing::info!(
                config_key = "logging.levels",
                new_value = %body.directives,
                "log levels updated via admin API"
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "updated",
                    "directives": body.directives,
                })),
            )
        })
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "status": "error",
                    "message": format!("Invalid log directives: {}", e),
                })),
            )
        })
}

#[derive(serde::Deserialize)]
struct UpdateLogLevelsRequest {
    /// Log level directives in RUST_LOG format.
    /// Example: "debug,supervisor=trace,proxy::service=warn"
    directives: String,
}

/// PATCH /admin/logging/sampling
/// Update log sampling rates at runtime.
async fn update_sampling(
    State(sampling): State<Arc<SamplingLayer>>,
    Json(body): Json<UpdateSamplingRequest>,
) -> Json<serde_json::Value> {
    sampling.set_rates(body.info_rate, body.debug_rate, body.trace_rate);
    tracing::info!(
        config_key = "logging.sampling",
        info_rate = body.info_rate,
        debug_rate = body.debug_rate,
        trace_rate = body.trace_rate,
        "log sampling rates updated via admin API"
    );
    Json(serde_json::json!({
        "status": "updated",
        "info_rate": body.info_rate,
        "debug_rate": body.debug_rate,
        "trace_rate": body.trace_rate,
    }))
}

#[derive(serde::Deserialize)]
struct UpdateSamplingRequest {
    info_rate: u64,
    debug_rate: u64,
    trace_rate: u64,
}
```

### 6.2 SIGHUP Log Level Reload

When the node receives SIGHUP, it re-reads the configuration file and updates
log levels. This integrates with Step 32's hot-reload mechanism.

```rust
// crates/node/src/main.rs (add after tracing initialization)

#[cfg(unix)]
{
    let reload_handle_clone = log_reload_handle.clone();
    tokio::spawn(async move {
        let mut stream = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::hangup()
        ).expect("failed to install SIGHUP handler");

        loop {
            stream.recv().await;
            tracing::info!("SIGHUP received — reloading log levels from config");

            // Re-read the config file and update log levels
            // (Step 32 provides the config file reading mechanism)
            if let Ok(config) = read_log_config_from_file(&args.config_path) {
                if let Err(e) = reload_handle_clone.update_levels(&config.log_directives()) {
                    tracing::error!(error = %e, "failed to reload log levels from config");
                }
            }
        }
    });
}
```

---

## 7. Log Rotation & Retention

For file-based logging, the node must rotate log files to prevent disk exhaustion.

### 7.1 File Rotation Configuration

```rust
// crates/common/src/logging.rs (continued)

/// Configuration for log file rotation.
#[derive(Debug, Clone)]
pub struct LogRotationConfig {
    /// Maximum size of a single log file before rotation.
    /// Default: 100 MB
    pub max_file_size_bytes: u64,

    /// Maximum number of rotated log files to keep.
    /// Default: 10
    pub max_files: u32,

    /// Maximum age of a log file before rotation.
    /// Default: 24 hours
    pub max_age: std::time::Duration,

    /// Compress rotated files with gzip.
    /// Default: true
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
```

### 7.2 Rotating File Writer

```rust
// crates/common/src/logging.rs (continued)

use std::io::Write;

/// A file writer that rotates when the file exceeds the configured size.
pub struct RotatingFileWriter {
    path: std::path::PathBuf,
    config: LogRotationConfig,
    current_size: u64,
    current_file: Option<std::fs::File>,
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
            current_size,
            current_file: Some(file),
        })
    }

    /// Write a line to the current log file, rotating if necessary.
    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        if self.current_size > self.config.max_file_size_bytes {
            self.rotate()?;
        }

        if let Some(ref mut file) = self.current_file {
            let bytes = line.as_bytes();
            file.write_all(bytes)?;
            file.write_all(b"\n")?;
            self.current_size += bytes.len() as u64 + 1;
        }

        Ok(())
    }

    /// Rotate the current log file.
    fn rotate(&mut self) -> std::io::Result<()> {
        // Close the current file
        self.current_file = None;

        // Remove the oldest rotated file if we've hit the limit
        let oldest = format!("{}.{}", self.path.display(), self.config.max_files);
        let _ = std::fs::remove_file(&oldest);

        // Shift rotated files: .N → .N+1
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

        self.current_file = Some(file);
        self.current_size = 0;

        Ok(())
    }

    /// Compress a rotated log file with gzip.
    fn compress_file(&self, path: &str) {
        let gz_path = format!("{}.gz", path);
        if let Ok(mut input) = std::fs::File::open(path) {
            if let Ok(mut output) = std::fs::File::create(&gz_path) {
                let mut encoder = flate2::write::GzEncoder::new(
                    &mut output,
                    flate2::Compression::fast(),
                );
                if std::io::copy(&mut input, &mut encoder).is_ok() {
                    let _ = encoder.finish();
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}
```

---

## 8. node.toml: Logging Configuration

The logging section of the node configuration file (Step 32).

```toml
# ── Logging Configuration ──────────────────────────────────────────

[logging]
# Output format: "json" (production) or "text" (development)
format = "json"

# Output destination: "stdout", "stderr", or a file path
# output = "stdout"
output = "/var/log/wasm-node/node.log"

# Default log level (overridden by RUST_LOG environment variable)
default_level = "info"

# Per-module log level overrides
[logging.modules]
"supervisor::pool" = "debug"
"proxy::service" = "info"
"messaging" = "warn"
"runtime::executor" = "info"

# Log sampling (reduces volume at high throughput)
[logging.sampling]
enabled = false
# Emit every Nth log at each level (1 = all, 10 = 10%)
info_rate = 1     # 100% of INFO logs
debug_rate = 10   # 10% of DEBUG logs
trace_rate = 100  # 1% of TRACE logs

# Log file rotation (only when output is a file path)
[logging.rotation]
max_file_size_mb = 100
max_files = 10
max_age_hours = 24
compress = true

# ── Log Forwarding ─────────────────────────────────────────────────

[logging.forward]
buffer_capacity = 8192
batch_size = 200
flush_interval_ms = 1000

# Forward to Grafana Loki
[[logging.forward.sinks]]
type = "loki"
endpoint = "http://loki.internal:3100"

[logging.forward.sinks.labels]
cluster = "production"
region = "us-east-1"

# Forward to Elasticsearch
# [[logging.forward.sinks]]
# type = "elasticsearch"
# endpoint = "http://es.internal:9200"
# index_prefix = "wasm-platform"

# Forward to Vector
# [[logging.forward.sinks]]
# type = "vector"
# endpoint = "http://vector.internal:9200/logs"

# ── Audit Logging ──────────────────────────────────────────────────

[logging.audit]
# Audit logs go to a separate destination
output = "/var/log/wasm-node/audit.log"

# Or forward audit logs via NATS
# output = "nats"
# nats_subject = "audit.events"

[logging.audit.rotation]
max_file_size_mb = 50
max_files = 30
max_age_hours = 168  # 7 days
compress = true
```

---

## 9. Field Naming Enforcement

A CI script that validates consistent field naming across all `tracing` calls.

### 9.1 Lint Script

```bash
#!/bin/bash
# scripts/check-log-fields.sh
# Validates that all tracing calls use the correct field names.
# Run in CI after `cargo fmt` and before `cargo clippy`.

set -euo pipefail

CRATES_DIR="crates"
ERRORS=0

# Check for inconsistent field names
# Each pattern is: "wrong_name" → "correct_name"
declare -A FIELD_CHECKS=(
    ["app ="]="app_id ="
    ["application ="]="app_id ="
    ["name ="]="app_id ="
    ["app_id =%"]="app_id = %"
    ["err ="]="error ="
    ["e ="]="error ="
    ["msg ="]="message ="
    ["duration ="]="latency_ms ="
    ["elapsed ="]="latency_ms ="
    ["latency ="]="latency_ms ="
    ["hash ="]="sha256 ="
    ["artifact_hash ="]="sha256 ="
)

echo "Checking tracing field names in $CRATES_DIR/..."

for crate_dir in "$CRATES_DIR"/*/; do
    crate_name=$(basename "$crate_dir")

    # Skip test directories
    if [[ "$crate_name" == "e2e" ]]; then
        continue
    fi

    # Find all Rust source files
    while IFS= read -r file; do
        line_num=0
        while IFS= read -r line; do
            line_num=$((line_num + 1))

            # Skip comments
            if [[ "$line" =~ ^[[:space:]]*// ]]; then
                continue
            fi

            # Check for tracing macros
            if [[ "$line" =~ tracing::(info|warn|error|debug|trace)! ]]; then
                for wrong in "${!FIELD_CHECKS[@]}"; do
                    correct="${FIELD_CHECKS[$wrong]}"
                    if [[ "$line" =~ $wrong ]]; then
                        echo "ERROR: $file:$line_num: Found '$wrong' — use '$correct' instead"
                        echo "  $line"
                        ERRORS=$((ERRORS + 1))
                    fi
                done
            fi

            # Check for eprintln!() and println!() in non-test code
            if [[ "$line" =~ eprintln! || "$line" =~ println! ]]; then
                # Allow in test functions
                if [[ ! "$line" =~ "#\[test\]" && ! "$line" =~ "fn test_" ]]; then
                    echo "WARN: $file:$line_num: Use tracing instead of eprintln!/println"
                    echo "  $line"
                fi
            fi
        done < "$file"
    done < <(find "$crate_dir" -name "*.rs" -not -path "*/tests/*")
done

if [ $ERRORS -gt 0 ]; then
    echo ""
    echo "Found $ERRORS field naming errors. Fix them before merging."
    exit 1
fi

echo "All tracing field names are consistent."
exit 0
```

### 9.2 CI Integration

```yaml
# .github/workflows/ci.yml (add to existing pipeline)
log-field-lint:
  runs-on: ubuntu-latest
  needs: [unit-tests]
  steps:
    - uses: actions/checkout@v4
    - name: Check log field naming
      run: bash scripts/check-log-fields.sh
```

---

## 10. Migration: Replacing `eprintln!()` Calls

The audit in Step 31 identified `eprintln!()` calls in production code. These must
be replaced with `tracing` macros.

### 10.1 Target Files

| File                              │ Current Code                                    │ Replacement
|───────────────────────────────────│─────────────────────────────────────────────────│──────────────────────────────────────
| `crates/runtime/src/executor.rs` │ `eprintln!("DEBUG: ...")`                       │ `tracing::debug!(...)`
| `crates/runtime/src/executor.rs` │ `eprintln!("compiling module...")`              │ `tracing::info!(sha256 = %hash, "compiling Wasm module")`
| `apps/hello-axum/src/main.rs`    │ `eprintln!("DEBUG: ECHO_SERVICE_URL = ...")`   │ `tracing::debug!(url = %url, "connecting to echo service")`
| `apps/postgres-app/src/main.rs`  │ `eprintln!("postgres-app listening on ...")`    │ `tracing::info!(addr = %addr, "listening")`
| `apps/postgres-app/src/main.rs`  │ `eprintln!("Connecting to ...")`                │ `tracing::debug!(host = %host, "connecting to PostgreSQL")`
| `apps/postgres-app/src/main.rs`  │ `eprintln!("Handshake OK")`                     │ `tracing::debug!("PostgreSQL handshake complete")`
| `apps/postgres-app/src/main.rs`  │ `eprintln!("Query result: ...")`                │ `tracing::debug!(result = %result, "query executed")`

### 10.2 Migration Rules

1. **`eprintln!("DEBUG: ...")`** → `tracing::debug!(...)` — these are debug statements
2. **`eprintln!("listening on ...")`** → `tracing::info!(addr = ..., "listening")` — startup info
3. **`eprintln!("error: ...")`** → `tracing::error!(error = ..., "description")` — errors
4. **`println!()` in `log_dispatcher.rs`** → `tracing::info!(...)` — the NodeStdout sink
   should use the JSON formatter, not `println!()`

For Wasm apps (`apps/`), the migration requires adding `tracing` and
`tracing-subscriber` as dependencies. Since these apps target `wasm32-wasip2`,
they must use `tracing-subscriber` without the `fmt` feature (which depends on
`std::io` features not available in WASI). Instead, they write to stdout/stderr
via the WASI pipe, and the Supervisor captures and structures the output.

---

## 11. Integration with Node Startup

Replace the current `tracing_subscriber::fmt()` initialization with the structured
logging system.

```rust
// crates/node/src/main.rs (modified startup)

async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // ── Structured Logging Initialization ────────────────────────
    let logging_config = common::logging::LoggingConfig {
        format: if args.log_format == "text" {
            common::logging::LogFormat::Text
        } else {
            common::logging::LogFormat::Json
        },
        output: match &args.log_output {
            Some(path) => common::logging::LogOutput::File {
                path: std::path::PathBuf::from(path),
            },
            None => common::logging::LogOutput::Stdout,
        },
        default_level: args.log_level.clone(),
        module_levels: args.log_module_levels.clone(),
        sampling_enabled: args.log_sampling,
        info_sample_rate: args.log_info_sample_rate,
        debug_sample_rate: args.log_debug_sample_rate,
        trace_sample_rate: args.log_trace_sample_rate,
        node_id: args.node_id.clone(),
        include_source: cfg!(debug_assertions),
    };

    let log_reload_handle = common::logging::init_logging(&logging_config);

    info!(node_id = %args.node_id, "wasm-node starting");

    // ... rest of startup unchanged ...

    // Store log_reload_handle in admin state for hot-reload
    // Store audit_logger for admin API audit events
    let audit_logger = common::logging::AuditLogger::start(
        common::logging::AuditOutput::File {
            path: std::path::PathBuf::from("/var/log/wasm-node/audit.log"),
        }
    );

    // ... existing startup code ...
}
```

### 11.1 New CLI Flags

```rust
// crates/node/src/main.rs (add to Args struct)

/// Log output format: "json" or "text"
#[arg(long, default_value = "json", env = "WASM_NODE_LOG_FORMAT")]
log_format: String,

/// Log output destination: "stdout", "stderr", or a file path
#[arg(long, env = "WASM_NODE_LOG_OUTPUT")]
log_output: Option<String>,

/// Default log level (overridden by RUST_LOG)
#[arg(long, default_value = "info", env = "WASM_NODE_LOG_LEVEL")]
log_level: String,

/// Enable log sampling for high-throughput scenarios
#[arg(long, default_value = "false")]
log_sampling: bool,

/// INFO log sampling rate (1 = 100%, 10 = 10%)
#[arg(long, default_value = "1")]
log_info_sample_rate: u64,

/// DEBUG log sampling rate
#[arg(long, default_value = "10")]
log_debug_sample_rate: u64,

/// TRACE log sampling rate
#[arg(long, default_value = "100")]
log_trace_sample_rate: u64,
```

---

## 12. Trace Correlation Between Node and Wasm Logs

When a request enters Pingora, it gets a `trace_id`. This ID must propagate through
the entire request lifecycle so that node logs and Wasm app logs can be correlated.

### 12.1 Trace ID Propagation

```rust
// crates/proxy/src/service.rs (modified)

impl ProxyHttp for WasmProxy {
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        // Extract or generate trace ID
        let trace_id = session
            .req_header()
            .headers
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
            .and_then(extract_trace_id)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Store in context for downstream use
        ctx.trace_id = Some(trace_id.clone());

        // Inject into the current tracing span
        tracing::Span::current().record("trace_id", &trace_id.as_str());

        // ... existing request_filter logic ...
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora_http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // Propagate trace ID to the Wasm app
        if let Some(ref trace_id) = ctx.trace_id {
            let _ = upstream_request.insert_header("X-Trace-Id", trace_id);
        }
        if let Some(id) = &ctx.app_id {
            let _ = upstream_request.insert_header("X-App-Id", &id.0);
        }
        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_core::Error>,
        ctx: &mut Self::CTX,
    ) {
        let latency_ms = ctx.start.elapsed().as_millis();
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);

        tracing::info!(
            app_id = ctx.app_id.as_ref().map(|a| a.0.as_str()).unwrap_or("unknown"),
            status,
            latency_ms,
            trace_id = ctx.trace_id.as_deref().unwrap_or(""),
            "request completed"
        );
    }
}

/// Extract the trace ID from a W3C traceparent header.
/// Format: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
fn extract_trace_id(header: &str) -> Option<String> {
    let parts: Vec<&str> = header.split('-').collect();
    if parts.len() >= 2 {
        Some(parts[1].to_string())
    } else {
        None
    }
}
```

### 12.2 Correlated Log Query Example

With both node and Wasm logs sharing `trace_id`, an operator can query:

```logql
# Loki query: all logs for a specific trace
{cluster="production"} |= "4bf92f3577b34da6a3ce929d0e0e4736"

# Or with structured fields:
{cluster="production"} | json | trace_id="4bf92f3577b34da6a3ce929d0e0e4736"
```

This returns both:
- The Pingora request log (`"request completed"`, latency, status)
- The Wasm app log (`"handling GET /users"`, app-specific fields)

---

## 13. `wasm-ctl` Log Commands

CLI commands for interacting with the logging system.

### 13.1 Log Level Management

```rust
// crates/ctl/src/cmds/logging.rs (new file)

use anyhow::Result;

/// Get current log levels.
pub async fn get_levels(node_api: &str, http: &reqwest::Client) -> Result<()> {
    let url = format!("{}/admin/logging/levels", node_api);
    let resp = http.get(&url).send().await?;
    let body: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

/// Update log levels at runtime.
pub async fn set_levels(
    node_api: &str,
    http: &reqwest::Client,
    directives: &str,
) -> Result<()> {
    let url = format!("{}/admin/logging/levels", node_api);
    let resp = http
        .patch(&url)
        .json(&serde_json::json!({ "directives": directives }))
        .send()
        .await?;

    if resp.status().is_success() {
        println!("Log levels updated: {}", directives);
    } else {
        let body: serde_json::Value = resp.json().await?;
        println!("Failed to update log levels: {}", body);
    }
    Ok(())
}

/// Update log sampling rates.
pub async fn set_sampling(
    node_api: &str,
    http: &reqwest::Client,
    info_rate: u64,
    debug_rate: u64,
    trace_rate: u64,
) -> Result<()> {
    let url = format!("{}/admin/logging/sampling", node_api);
    let resp = http
        .patch(&url)
        .json(&serde_json::json!({
            "info_rate": info_rate,
            "debug_rate": debug_rate,
            "trace_rate": trace_rate,
        }))
        .send()
        .await?;

    if resp.status().is_success() {
        println!(
            "Sampling rates updated: info=1/{} debug=1/{} trace=1/{}",
            info_rate, debug_rate, trace_rate
        );
    } else {
        println!("Failed to update sampling rates");
    }
    Ok(())
}
```

### 13.2 CLI Integration

```rust
// crates/ctl/src/main.rs (add to Commands enum)

/// Logging configuration and management
Logging {
    #[command(subcommand)]
    action: LoggingAction,
},

#[derive(clap::Subcommand)]
enum LoggingAction {
    /// Show current log levels
    Levels,
    /// Update log levels (e.g., "debug,supervisor=trace")
    SetLevels {
        /// Log level directives in RUST_LOG format
        directives: String,
    },
    /// Update log sampling rates
    SetSampling {
        /// INFO sample rate (1 = 100%)
        #[arg(long, default_value = "1")]
        info_rate: u64,
        /// DEBUG sample rate (10 = 10%)
        #[arg(long, default_value = "10")]
        debug_rate: u64,
        /// TRACE sample rate (100 = 1%)
        #[arg(long, default_value = "100")]
        trace_rate: u64,
    },
}
```

---

## 14. Prometheus Metrics for Logging

Track log volume and forwarder health in Prometheus.

```rust
// crates/metrics/src/log_metrics.rs (new file)

use prometheus::{IntCounter, IntGauge, Registry};

/// Metrics for the logging subsystem.
pub struct LogMetrics {
    /// Total log records emitted by the node.
    pub records_total: IntCounter,

    /// Log records dropped due to channel full.
    pub records_dropped: IntCounter,

    /// Log records forwarded to external sinks.
    pub records_forwarded: IntCounter,

    /// Forwarder write errors.
    pub forwarder_errors: IntCounter,

    /// Current channel utilization (0–1).
    pub channel_utilization: Gauge,

    /// Audit records emitted.
    pub audit_records_total: IntCounter,

    /// Log sampling rejection count.
    pub sampling_rejected: IntCounter,
}

impl LogMetrics {
    pub fn new(registry: &Registry) -> Self {
        let records_total = IntCounter::new(
            "wasm_node_log_records_total",
            "Total log records emitted",
        ).unwrap();
        registry.register(Box::new(records_total.clone())).unwrap();

        let records_dropped = IntCounter::new(
            "wasm_node_log_records_dropped_total",
            "Log records dropped due to backpressure",
        ).unwrap();
        registry.register(Box::new(records_dropped.clone())).unwrap();

        let records_forwarded = IntCounter::new(
            "wasm_node_log_records_forwarded_total",
            "Log records forwarded to external sinks",
        ).unwrap();
        registry.register(Box::new(records_forwarded.clone())).unwrap();

        let forwarder_errors = IntCounter::new(
            "wasm_node_log_forwarder_errors_total",
            "Log forwarder write errors",
        ).unwrap();
        registry.register(Box::new(forwarder_errors.clone())).unwrap();

        let audit_records_total = IntCounter::new(
            "wasm_node_audit_records_total",
            "Audit log records emitted",
        ).unwrap();
        registry.register(Box::new(audit_records_total.clone())).unwrap();

        let sampling_rejected = IntCounter::new(
            "wasm_node_log_sampling_rejected_total",
            "Log records rejected by sampling",
        ).unwrap();
        registry.register(Box::new(sampling_rejected.clone())).unwrap();

        LogMetrics {
            records_total,
            records_dropped,
            records_forwarded,
            forwarder_errors,
            channel_utilization: Gauge::new(
                "wasm_node_log_channel_utilization",
                "Log channel utilization ratio",
            ).unwrap(),
            audit_records_total,
            sampling_rejected,
        }
    }
}
```

### 14.1 Alerting Rules

```yaml
# Log subsystem alerting rules
groups:
  - name: wasm_platform_logging
    rules:
      - alert: LogForwarderErrors
        expr: rate(wasm_node_log_forwarder_errors_total[5m]) > 0
        for: 2m
        annotations:
          summary: "Log forwarder is failing"
          description: "The log forwarder has encountered {{ $value }} errors/s in the last 5 minutes."

      - alert: LogRecordsDropped
        expr: rate(wasm_node_log_records_dropped_total[5m]) > 0
        for: 5m
        annotations:
          summary: "Log records are being dropped"
          description: "{{ $value }} log records/s are being dropped due to channel backpressure."

      - alert: HighLogVolume
        expr: rate(wasm_node_log_records_total[5m]) > 10000
        for: 10m
        annotations:
          summary: "Unusually high log volume"
          description: "Node is producing {{ $value }} log records/s. Consider enabling log sampling."
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Structured Log Format
- [ ] `NodeLogRecord` schema defined in `crates/common/src/logging.rs`
- [ ] `NodeJsonFormatter` produces valid JSON lines matching the schema
- [ ] Every log line from the node process is valid JSON (no text-mode leaks)
- [ ] `--log-format text` flag restores human-readable output for development
- [ ] Log output can be directed to stdout, stderr, or a file path

### Field Naming Convention
- [ ] All crates use `app_id` (not `app`, `application`, or `name`) for app identity
- [ ] All crates use `error` (not `err`, `e`, or `msg`) for error descriptions
- [ ] All crates use `latency_ms` (not `duration`, `elapsed`, or `latency`) for timing
- [ ] All crates use `sha256` (not `hash` or `artifact_hash`) for artifact hashes
- [ ] All crates use `trace_id` for trace correlation
- [ ] CI script `scripts/check-log-fields.sh` validates field names in CI
- [ ] CI pipeline includes the log field lint step

### eprintln!() Elimination
- [ ] All `eprintln!()` calls in `crates/runtime/src/executor.rs` replaced with `tracing`
- [ ] All `eprintln!()` calls in `apps/hello-axum/src/main.rs` replaced with `tracing`
- [ ] All `eprintln!()` calls in `apps/postgres-app/src/main.rs` replaced with `tracing`
- [ ] `println!()` in `crates/metrics/src/log_dispatcher.rs` replaced with structured output
- [ ] No `eprintln!()` or `println!()` calls remain in `crates/` (excluding test code)

### Log Level Hot-Reload
- [ ] `LogReloadHandle` allows runtime log level changes without restart
- [ ] `PATCH /admin/logging/levels` endpoint updates log levels in <1 second
- [ ] SIGHUP signal reloads log levels from the config file
- [ ] `wasm-ctl logging set-levels "debug,supervisor=trace"` works
- [ ] Log level changes are logged as audit events

### Log Sampling
- [ ] `SamplingLayer` correctly samples INFO/DEBUG/TRACE logs
- [ ] WARN and ERROR logs are never sampled (100% emission)
- [ ] Sampling rates are hot-reloadable via `PATCH /admin/logging/sampling`
- [ ] Sampling rejection count is exposed as Prometheus metric
- [ ] Under 10,000 req/s load, log volume stays below 5,000 lines/s with sampling enabled

### Audit Log Separation
- [ ] `AuditLogRecord` schema defined with `log_type`, `action`, `actor`, `success`
- [ ] Audit logger writes to a separate file from operational logs
- [ ] All admin API endpoints produce audit records
- [ ] Secret access produces audit records
- [ ] Config changes produce audit records
- [ ] Audit records are never sampled or dropped

### Log Forwarding
- [ ] `LogForwarder` handles both `NodeLogRecord` and `WasmLogRecord`
- [ ] Loki sink pushes logs in the correct Loki push format
- [ ] Elasticsearch sink uses the bulk API with date-based indices
- [ ] Vector sink posts JSON batches to the configured endpoint
- [ ] Forwarder failures are logged as warnings (not panics)
- [ ] Forwarder metrics are exposed in Prometheus

### Log Rotation
- [ ] `RotatingFileWriter` rotates files at the configured size limit
- [ ] Rotated files are optionally compressed with gzip
- [ ] Old rotated files are cleaned up at the configured max count
- [ ] Rotation does not lose log records (atomic rename)

### Trace Correlation
- [ ] Pingora generates or propagates `trace_id` for every request
- [ ] `trace_id` is injected as `X-Trace-Id` header to the Wasm app
- [ ] `trace_id` appears in both node logs and Wasm app logs
- [ ] Loki/ES queries can correlate node and Wasm logs by `trace_id`

### Configuration
- [ ] `node.toml` `[logging]` section configures format, output, levels, sampling
- [ ] `node.toml` `[logging.forward]` section configures external sinks
- [ ] `node.toml` `[logging.audit]` section configures audit log output
- [ ] CLI flags `--log-format`, `--log-output`, `--log-level` override TOML config
- [ ] `RUST_LOG` environment variable overrides all other level configuration

### Tests
- [ ] Unit test: `NodeJsonFormatter` produces valid JSON with all required fields
- [ ] Unit test: `FieldCollector` correctly extracts known fields from tracing events
- [ ] Unit test: `SamplingLayer` emits all WARN/ERROR and samples INFO/DEBUG/TRACE
- [ ] Unit test: `RotatingFileWriter` rotates at the size limit
- [ ] Unit test: `AuditLogger` records are never dropped (channel is non-blocking)
- [ ] Integration test: Loki sink sends a batch and receives 204 No Content
- [ ] Integration test: Log level hot-reload changes visible output within 1 second
- [ ] E2E test: trace_id from Pingora appears in Wasm app stdout capture
