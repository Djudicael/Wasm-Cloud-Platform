use thiserror::Error;

/// Platform-wide error type with optional source chains.
///
/// Each variant carries a human-readable `message` and an optional `source`
/// that preserves the original error for debugging and error-chain inspection.
///
/// # Constructing errors
///
/// Convenience constructors are provided for each variant:
///
/// ```
/// use common::error::PlatformError;
///
/// // Simple message (equivalent to the old PlatformError::Storage("msg".into()))
/// let err = PlatformError::storage("disk full");
///
/// // With a source error — the source message is used as the display message
/// let err = PlatformError::storage_source(std::io::Error::new(
///     std::io::ErrorKind::BrokenPipe, "oops",
/// ));
///
/// // With both an explicit message and a source
/// let err = PlatformError::storage_with_msg(
///     "failed to open database",
///     std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
/// );
/// ```
#[derive(Error, Debug)]
pub enum PlatformError {
    #[error("Storage error: {message}")]
    Storage {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Runtime error: {message}")]
    Runtime {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Fuel exhausted for app {app_id}")]
    FuelExhausted { app_id: String },

    #[error("Memory limit exceeded for app {app_id}")]
    MemoryLimitExceeded { app_id: String },

    #[error("App not found: {0}")]
    AppNotFound(String),

    #[error("Instance not found: {0}")]
    InstanceNotFound(String),

    #[error("Encryption error: {message}")]
    Encryption {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Messaging error: {message}")]
    Messaging {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Proxy error: {message}")]
    Proxy {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Config validation error: {0}")]
    ConfigValidation(String),

    #[error("Network error: {message}")]
    Network {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Security error: {0}")]
    Security(String),

    #[error("IO error: {message}")]
    Io {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("External service error: {message}")]
    External {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
}

// ── Convenience constructors ──────────────────────────────────────────────────
//
// Each variant that carries a `source` field gets three constructors:
//
//   1. `variant(msg)`            — message only, source = None
//   2. `variant_source(err)`     — source only, message = err.to_string()
//   3. `variant_with_msg(msg, err)` — explicit message + source
//
// Variants without a source field get a single constructor.

impl PlatformError {
    // -- Storage ---------------------------------------------------------------

    pub fn storage(msg: impl Into<String>) -> Self {
        PlatformError::Storage {
            message: msg.into(),
            source: None,
        }
    }

    pub fn storage_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        PlatformError::Storage {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    pub fn storage_with_msg(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        PlatformError::Storage {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }

    // -- Runtime ---------------------------------------------------------------

    pub fn runtime(msg: impl Into<String>) -> Self {
        PlatformError::Runtime {
            message: msg.into(),
            source: None,
        }
    }

    pub fn runtime_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        PlatformError::Runtime {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    pub fn runtime_with_msg(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        PlatformError::Runtime {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }

    // -- Encryption ------------------------------------------------------------

    pub fn encryption(msg: impl Into<String>) -> Self {
        PlatformError::Encryption {
            message: msg.into(),
            source: None,
        }
    }

    pub fn encryption_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        PlatformError::Encryption {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    pub fn encryption_with_msg(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        PlatformError::Encryption {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }

    // -- Messaging -------------------------------------------------------------

    pub fn messaging(msg: impl Into<String>) -> Self {
        PlatformError::Messaging {
            message: msg.into(),
            source: None,
        }
    }

    pub fn messaging_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        PlatformError::Messaging {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    pub fn messaging_with_msg(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        PlatformError::Messaging {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }

    // -- Proxy -----------------------------------------------------------------

    pub fn proxy(msg: impl Into<String>) -> Self {
        PlatformError::Proxy {
            message: msg.into(),
            source: None,
        }
    }

    pub fn proxy_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        PlatformError::Proxy {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    pub fn proxy_with_msg(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        PlatformError::Proxy {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }

    // -- Network ---------------------------------------------------------------

    pub fn network(msg: impl Into<String>) -> Self {
        PlatformError::Network {
            message: msg.into(),
            source: None,
        }
    }

    pub fn network_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        PlatformError::Network {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    pub fn network_with_msg(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        PlatformError::Network {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }

    // -- IO --------------------------------------------------------------------

    pub fn io(msg: impl Into<String>) -> Self {
        PlatformError::Io {
            message: msg.into(),
            source: None,
        }
    }

    pub fn io_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        PlatformError::Io {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    pub fn io_with_msg(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        PlatformError::Io {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }

    // -- External --------------------------------------------------------------

    pub fn external(msg: impl Into<String>) -> Self {
        PlatformError::External {
            message: msg.into(),
            source: None,
        }
    }

    pub fn external_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        PlatformError::External {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    pub fn external_with_msg(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        PlatformError::External {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }

    // -- Simple variants (no source field) -------------------------------------

    pub fn config_validation(msg: impl Into<String>) -> Self {
        PlatformError::ConfigValidation(msg.into())
    }

    pub fn security(msg: impl Into<String>) -> Self {
        PlatformError::Security(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        PlatformError::Internal(msg.into())
    }
}

// ── From impls for common error types ─────────────────────────────────────────

impl From<std::io::Error> for PlatformError {
    fn from(err: std::io::Error) -> Self {
        PlatformError::io_source(err)
    }
}

impl From<serde_json::Error> for PlatformError {
    fn from(err: serde_json::Error) -> Self {
        PlatformError::storage_with_msg("serialization/deserialization failed", err)
    }
}

// Note: From<redb::*> impls are intentionally omitted here to avoid a
// circular dependency (common → redb → common).  Call sites that need to
// convert redb errors should use `PlatformError::storage_source(e)` or
// `PlatformError::storage_with_msg("context", e)` explicitly.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_construction() {
        let err = PlatformError::storage("disk full");
        assert_eq!(err.to_string(), "Storage error: disk full");
    }

    #[test]
    fn test_source_chain() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = PlatformError::storage_with_msg("cannot open database", io_err);
        assert_eq!(err.to_string(), "Storage error: cannot open database");

        // The source chain is preserved
        let source = match &err {
            PlatformError::Storage { source, .. } => source,
            _ => unreachable!(),
        };
        assert!(source.is_some());
        assert!(source
            .as_ref()
            .unwrap()
            .to_string()
            .contains("file missing"));
    }

    #[test]
    fn test_source_only_constructor() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broke");
        let err = PlatformError::storage_source(io_err);
        // Message should be derived from the source
        assert!(err.to_string().contains("pipe broke"));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err: PlatformError = io_err.into();
        assert!(err.to_string().contains("access denied"));
    }

    #[test]
    fn test_simple_variants() {
        let err = PlatformError::config_validation("invalid port");
        assert_eq!(err.to_string(), "Config validation error: invalid port");

        let err = PlatformError::security("hash mismatch");
        assert_eq!(err.to_string(), "Security error: hash mismatch");

        let err = PlatformError::internal("unexpected state");
        assert_eq!(err.to_string(), "Internal error: unexpected state");
    }

    #[test]
    fn test_no_source_variants() {
        let err = PlatformError::FuelExhausted {
            app_id: "my-app:v1".to_string(),
        };
        assert_eq!(err.to_string(), "Fuel exhausted for app my-app:v1");

        let err = PlatformError::AppNotFound("test".to_string());
        assert_eq!(err.to_string(), "App not found: test");
    }
}
