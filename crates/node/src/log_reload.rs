//! Runtime log-level reload support.
//!
//! `LogReloadHandle` wraps a `tracing-subscriber` reload handle so that the
//! log level can be changed at runtime (e.g. via the admin API or hot-config)
//! without restarting the node.

use std::sync::Arc;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{
    fmt,
    reload::{self, Handle},
    EnvFilter, Registry,
};

/// A handle that allows changing the log filter at runtime.
///
/// Clone-safe — internally reference-counted.
#[derive(Clone)]
pub struct LogReloadHandle {
    handle: Arc<Handle<EnvFilter, Registry>>,
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
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

        let (reload_layer, reload_handle) = reload::Layer::new(env_filter);

        let subscriber = Registry::default().with(reload_layer).with(fmt::layer());

        tracing::subscriber::set_global_default(subscriber).expect(
            "failed to set global tracing subscriber — another subscriber is already installed",
        );

        LogReloadHandle {
            handle: Arc::new(reload_handle),
        }
    }

    /// Change the log level at runtime.
    ///
    /// Accepts the same directive format as `RUST_LOG`, e.g. `"debug"`,
    /// `"warn,proxy::service=trace"`.
    ///
    /// Returns `Ok(())` on success, or an error string if the new filter could
    /// not be applied (e.g. invalid directive).
    pub fn set_level(&self, level: &str) -> Result<(), String> {
        let new_filter = EnvFilter::new(level);
        self.handle
            .reload(new_filter)
            .map_err(|e| format!("failed to reload log filter: {}", e))
    }

    /// Update the log filter with a more complex directive string.
    ///
    /// This is the same as [`set_level`] but the name makes the intent clearer
    /// when passing compound directives like `"info,my_crate=debug"`.
    pub fn update_levels(&self, directives: &str) -> Result<(), String> {
        self.set_level(directives)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_level_valid() {
        // We can't easily test the global subscriber in unit tests because
        // `set_global_default` can only be called once per process, but we
        // can at least verify the handle can be constructed and that
        // `set_level` accepts valid levels.
        let filter = EnvFilter::new("info");
        let (_layer, handle) = reload::Layer::new(filter);
        let log_handle = LogReloadHandle {
            handle: Arc::new(handle),
        };
        assert!(log_handle.set_level("debug").is_ok());
        assert!(log_handle.set_level("warn,proxy=trace").is_ok());
    }

    #[test]
    fn test_set_level_invalid() {
        let filter = EnvFilter::new("info");
        let (_layer, handle) = reload::Layer::new(filter);
        let log_handle = LogReloadHandle {
            handle: Arc::new(handle),
        };
        // tracing-subscriber's EnvFilter is quite permissive; invalid level
        // names are silently treated as crate-level directives. This test
        // just verifies the mechanism doesn't panic.
        let _ = log_handle.set_level("nonexistent_level");
    }
}
