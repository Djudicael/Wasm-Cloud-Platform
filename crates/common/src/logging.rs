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
pub use init::{init_logging, LogReloadHandle};
pub use rotation::{LogRotationConfig, RotatingFileWriter};
pub use sampling::SamplingLayer;

#[cfg(test)]
pub(crate) use format::FieldCollector;
#[cfg(test)]
pub(crate) use init::build_log_writer;

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
