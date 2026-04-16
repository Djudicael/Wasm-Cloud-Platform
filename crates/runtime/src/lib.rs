pub mod compiler;
pub mod custom_pipe;
pub mod executor;
pub mod limits;

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

impl Default for WasmRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmRuntime {
    pub fn new() -> Self {
        WasmRuntime {
            engine: Arc::new(compiler::build_engine()),
        }
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
        PreparedModule::from_artifact(self.engine.as_ref().clone(), artifact_bytes, config)
    }
}
