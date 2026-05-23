pub mod artifact_transfer;
pub mod auth;
pub mod billing;
pub mod config;
pub mod crypto;
pub mod error;
pub mod gc;
pub mod health;
pub mod logging;

pub mod policy;
pub mod protocol;
pub mod types;
pub mod upgrade_provenance;

/// The internal gateway port for East-West traffic on the loopback interface.
///
/// Both the Supervisor and the internal gateway reference this constant so they
/// agree on which port the gateway listens on.
pub const INTERNAL_GATEWAY_PORT: u16 = 9080;
