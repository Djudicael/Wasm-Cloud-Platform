use common::{config::RuntimeSection, error::PlatformError};
use std::path::{Path, PathBuf};
use wasmtime::component::Component;
use wasmtime::{Cache, Config, Engine};

fn configure_code_cache(
    config: &mut Config,
    runtime: Option<&RuntimeSection>,
) -> Result<Option<PathBuf>, PlatformError> {
    let Some(cache_dir) = runtime.and_then(|cfg| cfg.cache_directory.as_ref()) else {
        return Ok(None);
    };

    let cache_dir_path = PathBuf::from(cache_dir);
    std::fs::create_dir_all(&cache_dir_path).map_err(|e| {
        PlatformError::runtime(format!(
            "failed to create Wasmtime cache directory {}: {}",
            cache_dir_path.display(),
            e
        ))
    })?;

    let config_path = cache_dir_path.join("wasmtime-cache-config.toml");
    let config_body = format!("[cache]\ndirectory = '{}'\n", cache_dir_path.display());
    std::fs::write(&config_path, config_body).map_err(|e| {
        PlatformError::runtime(format!(
            "failed to write Wasmtime cache config {}: {}",
            config_path.display(),
            e
        ))
    })?;

    let cache = Cache::from_file(Some(Path::new(&config_path))).map_err(|e| {
        PlatformError::runtime(format!(
            "failed to load Wasmtime cache config {}: {}",
            config_path.display(),
            e
        ))
    })?;
    config.cache(Some(cache));
    Ok(Some(cache_dir_path))
}

/// Build a Cranelift-based AOT engine.
/// Call once per process and share via Arc.
pub fn build_engine(runtime: Option<&RuntimeSection>) -> Result<Engine, PlatformError> {
    let mut config = Config::new();

    // Enable fuel metering for execution limits
    config.consume_fuel(true);

    // Enable coarse-grained epoch interruption so long-running guests can be
    // trapped even when fuel settings are generous or disabled in the future.
    config.epoch_interruption(true);

    // Optimize for execution speed (AOT compilation)
    config.cranelift_opt_level(wasmtime::OptLevel::Speed);

    // Enable Component Model
    config.wasm_component_model(true);

    let cache_dir = configure_code_cache(&mut config, runtime)?;
    if let Some(path) = cache_dir.as_ref() {
        tracing::info!(path = %path.display(), "Wasmtime code cache enabled");
    }

    Engine::new(&config)
        .map_err(|e| PlatformError::runtime(format!("Failed to create Wasmtime Engine: {}", e)))
}

/// Compile raw `.wasm` bytes into a native artifact.
/// This is CPU-intensive — run on a blocking thread (tokio::task::spawn_blocking).
///
/// Returns: serialized artifact bytes (store in redb).
pub fn compile(engine: &Engine, wasm_bytes: &[u8]) -> Result<Vec<u8>, PlatformError> {
    let component = Component::new(engine, wasm_bytes)
        .map_err(|e| PlatformError::runtime(format!("compile error: {e}")))?;

    // Serialize the compiled component to bytes (portable Artifact format).
    let artifact = component
        .serialize()
        .map_err(|e| PlatformError::runtime(format!("serialize error: {e}")))?;

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
        .map_err(|e| PlatformError::runtime(format!("deserialize error: {e}")))
}
