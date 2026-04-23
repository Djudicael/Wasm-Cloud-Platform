//! Runtime log-level reload support.
//!
//! Thin wrapper around [`common::logging`] that bridges the node startup path
//! with the structured logging subsystem.

use common::logging::{init_logging, LoggingConfig};

/// A handle that allows changing the log filter at runtime.
///
/// Clone-safe — internally reference-counted.
#[derive(Clone)]
pub struct LogReloadHandle {
    inner: common::logging::LogReloadHandle,
}

impl LogReloadHandle {
    /// Initialise the global tracing subscriber and return a reload handle.
    ///
    /// Call this **once** during startup, then pass the returned handle to any
    /// component that needs to adjust the log level at runtime (admin API,
    /// hot-config, etc.).
    ///
    /// If the `RUST_LOG` environment variable is set it takes precedence over
    /// `default_level`, matching the standard `tracing-subscriber` behaviour.
    pub fn init(default_level: &str) -> Self {
        let config = LoggingConfig {
            default_level: default_level.to_string(),
            ..LoggingConfig::default()
        };
        Self {
            inner: init_logging(&config),
        }
    }

    /// Change the log level at runtime.
    ///
    /// Accepts the same directive format as `RUST_LOG`, e.g. `"debug"`,
    /// `"warn,proxy::service=trace"`.
    pub fn set_level(&self, level: &str) -> Result<(), String> {
        self.inner.update_levels(level)
    }

    /// Update the log filter with a more complex directive string.
    ///
    /// This is the same as [`set_level`] but the name makes the intent clearer
    /// when passing compound directives like `"info,my_crate=debug"`.
    pub fn update_levels(&self, directives: &str) -> Result<(), String> {
        self.inner.update_levels(directives)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_level_valid() {
        // init_logging can only be called once per process, so we construct
        // the handle manually without installing a global subscriber.
        let env_filter = tracing_subscriber::EnvFilter::new("info");
        let (_layer, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);
        let handle = LogReloadHandle {
            inner: common::logging::LogReloadHandle::new(reload_handle),
        };
        assert!(handle.set_level("debug").is_ok());
        assert!(handle.set_level("warn,proxy=trace").is_ok());
    }

    #[test]
    fn test_set_level_invalid() {
        let env_filter = tracing_subscriber::EnvFilter::new("info");
        let (_layer, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);
        let handle = LogReloadHandle {
            inner: common::logging::LogReloadHandle::new(reload_handle),
        };
        // tracing-subscriber's EnvFilter is quite permissive; invalid level
        // names are silently treated as crate-level directives. This test
        // just verifies the mechanism doesn't panic.
        let _ = handle.set_level("nonexistent_level");
    }
}
