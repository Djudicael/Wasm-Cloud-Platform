//! Configuration management for the Wasm Cloud Platform.
//!
//! This crate is split by responsibility:
//! - `overrides`: CLI-provided overrides
//! - `loader`: config loading and merge layering
//! - `validation`: final config validation helpers
//! - `hot`: hot-reloadable config state and persistence

mod hot;
mod loader;
mod overrides;
mod validation;

pub use hot::{HotConfig, HotConfigHandle, HotConfigUpdate};
pub use loader::load_config;
pub use overrides::CliOverrides;

#[cfg(test)]
mod tests;
