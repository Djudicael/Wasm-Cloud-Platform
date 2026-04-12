use common::error::PlatformError;
use wasmtime::component::Component;
use wasmtime::{Config, Engine};

/// Build a Cranelift-based AOT engine.
/// Call once per process and share via Arc.
pub fn build_engine() -> Engine {
    let mut config = Config::new();

    // Enable fuel metering for execution limits
    config.consume_fuel(true);

    // Optimize for execution speed (AOT compilation)
    config.cranelift_opt_level(wasmtime::OptLevel::Speed);

    // Enable Component Model
    config.wasm_component_model(true);

    Engine::new(&config).expect("Failed to create Wasmtime Engine")
}

/// Compile raw `.wasm` bytes into a native artifact.
/// This is CPU-intensive — run on a blocking thread (tokio::task::spawn_blocking).
///
/// Returns: serialized artifact bytes (store in redb).
pub fn compile(engine: &Engine, wasm_bytes: &[u8]) -> Result<Vec<u8>, PlatformError> {
    let component = Component::new(engine, wasm_bytes)
        .map_err(|e| PlatformError::Runtime(format!("compile error: {e}")))?;

    // Serialize the compiled component to bytes (portable Artifact format).
    let artifact = component
        .serialize()
        .map_err(|e| PlatformError::Runtime(format!("serialize error: {e}")))?;

    Ok(artifact.to_vec())
}

/// Deserialize a stored artifact back to a Component.
/// This is near-instant (<1ms for most apps).
///
/// # Safety
/// `artifact_bytes` must be produced by `compile()` with a compatible engine.
pub unsafe fn deserialize(
    engine: &Engine,
    artifact_bytes: &[u8],
) -> Result<Component, PlatformError> {
    Component::deserialize(engine, artifact_bytes)
        .map_err(|e| PlatformError::Runtime(format!("deserialize error: {e}")))
}
