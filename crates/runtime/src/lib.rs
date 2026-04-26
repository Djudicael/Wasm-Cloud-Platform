pub mod compiler;
pub mod executor;
pub mod limits;
pub mod policy_tracker;
pub mod policy_wasi;
pub mod virtual_dns;

#[cfg(test)]
mod tests;

use common::{error::PlatformError, types::AppConfig};
use executor::PreparedModule;
use std::sync::Arc;
use wasmtime::Engine;

/// High-level runtime handle shared across the node.
#[derive(Clone)]
pub struct WasmRuntime {
    pub engine: Arc<Engine>,
}

impl WasmRuntime {
    /// Create a new WasmRuntime with a Cranelift-based AOT engine.
    /// Returns an error if the engine fails to initialize.
    pub fn new() -> Result<Self, PlatformError> {
        let engine = compiler::build_engine()?;
        Ok(WasmRuntime {
            engine: Arc::new(engine),
        })
    }

    /// Compile raw `.wasm` bytes to a serializable artifact.
    /// Run this in `tokio::task::spawn_blocking` — it is CPU-intensive.
    pub fn compile(&self, wasm_bytes: &[u8]) -> Result<Vec<u8>, PlatformError> {
        compiler::compile(&self.engine, wasm_bytes)
    }

    /// Prepare a stored artifact for execution (near-instant).
    pub fn prepare(
        &self,
        artifact_bytes: &[u8],
        config: AppConfig,
    ) -> Result<PreparedModule, PlatformError> {
        PreparedModule::from_artifact(self.engine.clone(), artifact_bytes, config)
    }
}
