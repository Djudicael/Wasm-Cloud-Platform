pub mod compiler;
pub mod executor;
pub mod limits;
mod policy_boundary;
pub mod policy_tracker;
pub mod virtual_dns;

#[cfg(test)]
mod tests;

use common::{config::RuntimeSection, error::PlatformError, types::AppConfig};
use executor::PreparedModule;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::Engine;

pub use policy_boundary::{
    current_policy_boundary, PolicyBoundaryCapability, PolicyEnforcementLayer,
    POLICY_BOUNDARY_CAPABILITIES,
};

const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);

fn start_epoch_thread(engine: &Engine) {
    let weak = engine.weak();
    std::thread::Builder::new()
        .name("wasmtime-epoch-ticker".to_string())
        .spawn(move || {
            while let Some(engine) = weak.upgrade() {
                std::thread::sleep(EPOCH_TICK_INTERVAL);
                engine.increment_epoch();
            }
        })
        .expect("failed to spawn wasmtime epoch ticker thread");
}

/// High-level runtime handle shared across the node.
#[derive(Clone)]
pub struct WasmRuntime {
    pub engine: Arc<Engine>,
}

impl WasmRuntime {
    /// Create a new WasmRuntime with a Cranelift-based AOT engine.
    /// Returns an error if the engine fails to initialize.
    pub fn new() -> Result<Self, PlatformError> {
        Self::new_with_runtime_config(None)
    }

    /// Create a new WasmRuntime using optional runtime configuration.
    pub fn new_with_runtime_config(
        runtime: Option<&RuntimeSection>,
    ) -> Result<Self, PlatformError> {
        let engine = compiler::build_engine(runtime)?;
        start_epoch_thread(&engine);
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
