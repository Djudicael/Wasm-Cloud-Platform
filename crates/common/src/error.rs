use thiserror::Error;
#[derive(Error, Debug)]
pub enum PlatformError {
    #[error("Storage error: {0}")]
    Storage(String),
    #[error("Runtime error: {0}")]
    Runtime(String),
    #[error("Fuel exhausted for app {app_id}")]
    FuelExhausted { app_id: String },
    #[error("Memory limit exceeded for app {app_id}")]
    MemoryLimitExceeded { app_id: String },
    #[error("App not found: {0}")]
    AppNotFound(String),
    #[error("Instance not found: {0}")]
    InstanceNotFound(String),
    #[error("Encryption error: {0}")]
    Encryption(String),
    #[error("Messaging error: {0}")]
    Messaging(String),
    #[error("Proxy error: {0}")]
    Proxy(String),
    #[error("Config validation error: {0}")]
    ConfigValidation(String),
}
