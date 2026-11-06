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

#[path = "logging/audit.rs"]
mod audit;
#[path = "logging/format.rs"]
mod format;
#[path = "logging/forwarder.rs"]
mod forwarder;
#[path = "logging/init.rs"]
mod init;
#[path = "logging/rotation.rs"]
mod rotation;
#[path = "logging/sampling.rs"]
mod sampling;

pub use audit::{AuditLogRecord, AuditLogger, AuditOutput};
pub use format::{LogFormat, LogOutput, LoggingConfig, NodeJsonFormatter, NodeLogRecord};
pub use forwarder::{ForwarderSinkConfig, LogForwarderConfig};
pub use init::{build_log_writer, init_logging, LogReloadHandle, LogWriter};
pub use rotation::{LogRotationConfig, RotatingFileWriter};
pub use sampling::SamplingLayer;

/// async-nats 0.50 traces the complete CONNECT structure, including password,
/// token and JWT. Exclude that one credential-bearing event before any log or
/// telemetry layer sees its fields, even when runtime logging changes to TRACE.
pub fn safe_sdk_event(metadata: &tracing::Metadata<'_>) -> bool {
    !(metadata.target() == "async_nats::connection"
        && metadata.fields().field("connect_info").is_some())
}

/// Record the HTTP semantic-convention attribute as an OTLP integer.
/// The pinned tracing-opentelemetry SpanVisitor routes unsigned values through
/// debug formatting, producing strings. Zero denotes no written response and
/// must leave the HTTP status attribute unset.
pub fn record_http_response_status(span: &tracing::Span, status: u16) {
    if status != 0 {
        span.record("http.response.status_code", i64::from(status));
    }
}

#[cfg(test)]
pub(crate) use format::FieldCollector;

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
