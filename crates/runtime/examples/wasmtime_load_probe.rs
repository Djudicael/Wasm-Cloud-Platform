use common::{
    config::RuntimeSection,
    types::{AppConfig, AppId},
};
use runtime::WasmRuntime;
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Baseline,
    Cache,
    Pooling,
    CachePooling,
}

impl Scenario {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "baseline" => Ok(Self::Baseline),
            "cache" => Ok(Self::Cache),
            "pooling" => Ok(Self::Pooling),
            "cache-pooling" => Ok(Self::CachePooling),
            other => Err(format!(
                "unknown scenario {other}; expected one of: baseline, cache, pooling, cache-pooling"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Cache => "cache",
            Self::Pooling => "pooling",
            Self::CachePooling => "cache-pooling",
        }
    }

    fn cache_enabled(self) -> bool {
        matches!(self, Self::Cache | Self::CachePooling)
    }

    fn pooling_enabled(self) -> bool {
        matches!(self, Self::Pooling | Self::CachePooling)
    }
}

#[derive(Debug)]
struct ProbeOptions {
    scenario: Scenario,
    component_path: PathBuf,
    sequential_spawns: u32,
    peak_live_instances: u32,
}

#[derive(Debug)]
struct ProbeResult {
    scenario: Scenario,
    cache_enabled: bool,
    pooling_enabled: bool,
    component_path: PathBuf,
    sequential_spawns: u32,
    peak_live_instances: u32,
    cold_compile_ms: u128,
    warm_compile_ms: Option<u128>,
    prepare_ms: u128,
    sequential_spawn_total_ms: u128,
    peak_live_spawn_ms: u128,
    rss_before_bytes: Option<u64>,
    rss_after_compile_bytes: Option<u64>,
    rss_after_sequential_bytes: Option<u64>,
    rss_after_peak_live_bytes: Option<u64>,
    rss_high_water_bytes: Option<u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_args(env::args().skip(1).collect())?;
    let result = run_probe(&options)?;
    print_result(&result);
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<ProbeOptions, Box<dyn std::error::Error>> {
    let mut scenario = Scenario::Baseline;
    let mut component_path = default_component_path().ok_or_else(|| {
        "component path not provided and target/wasm32-wasip2/release/hello-axum.wasm was not found"
            .to_string()
    })?;
    let mut sequential_spawns = 64;
    let mut peak_live_instances = 32;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scenario" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "--scenario requires a value".to_string())?;
                scenario = Scenario::parse(value)?;
            }
            "--component" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "--component requires a value".to_string())?;
                component_path = PathBuf::from(value);
            }
            "--sequential-spawns" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "--sequential-spawns requires a value".to_string())?;
                sequential_spawns = value.parse::<u32>()?;
            }
            "--peak-live-instances" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "--peak-live-instances requires a value".to_string())?;
                peak_live_instances = value.parse::<u32>()?;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                return Err(format!("unknown argument: {other}").into());
            }
        }
        i += 1;
    }

    if sequential_spawns == 0 {
        return Err("--sequential-spawns must be > 0".into());
    }
    if peak_live_instances == 0 {
        return Err("--peak-live-instances must be > 0".into());
    }
    if !component_path.exists() {
        return Err(format!("component not found: {}", component_path.display()).into());
    }

    Ok(ProbeOptions {
        scenario,
        component_path,
        sequential_spawns,
        peak_live_instances,
    })
}

fn print_usage() {
    eprintln!("usage: cargo run -p runtime --example wasmtime_load_probe -- [options]");
    eprintln!("  --scenario <baseline|cache|pooling|cache-pooling>");
    eprintln!("  --component <path-to-component.wasm>");
    eprintln!("  --sequential-spawns <count>       default: 64");
    eprintln!("  --peak-live-instances <count>     default: 32");
}

fn default_component_path() -> Option<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let candidates = [
        workspace_root.join("target/wasm32-wasip2/release/hello-axum.wasm"),
        workspace_root.join("target/wasm32-wasip2/release/hello_axum.wasm"),
    ];
    candidates.into_iter().find(|path| path.exists())
}

fn runtime_config(
    scenario: Scenario,
    cache_directory: Option<&Path>,
    peak_live_instances: u32,
) -> RuntimeSection {
    let mut runtime = RuntimeSection::default();
    runtime.cache_directory = cache_directory.map(|path| path.display().to_string());
    runtime.pooling_allocator = scenario.pooling_enabled();
    runtime.pooling_total_component_instances = peak_live_instances.saturating_mul(2).max(64);
    runtime.pooling_max_core_instances_per_component =
        Some(peak_live_instances.saturating_mul(2).max(8));
    runtime.pooling_max_memories_per_component = Some(peak_live_instances.max(4));
    runtime.pooling_max_tables_per_component = Some(peak_live_instances.max(4));
    runtime
}

fn run_probe(options: &ProbeOptions) -> Result<ProbeResult, Box<dyn std::error::Error>> {
    let wasm_bytes = fs::read(&options.component_path)?;
    let rss_before_bytes = read_proc_status_bytes("VmRSS");

    let cache_root = if options.scenario.cache_enabled() {
        let path = std::env::temp_dir().join(format!(
            "wasmtime-load-probe-{}-{}",
            std::process::id(),
            options.scenario.as_str()
        ));
        if path.exists() {
            let _ = fs::remove_dir_all(&path);
        }
        fs::create_dir_all(&path)?;
        Some(path)
    } else {
        None
    };

    let cold_config = runtime_config(
        options.scenario,
        cache_root.as_deref(),
        options.peak_live_instances,
    );
    let cold_runtime = WasmRuntime::new_with_runtime_config(Some(&cold_config))?;

    let cold_compile_start = Instant::now();
    let cold_artifact = cold_runtime.compile(&wasm_bytes)?;
    let cold_compile_ms = cold_compile_start.elapsed().as_millis();
    drop(cold_runtime);

    let (runtime, artifact_bytes, warm_compile_ms) = if options.scenario.cache_enabled() {
        let warm_runtime = WasmRuntime::new_with_runtime_config(Some(&cold_config))?;
        let warm_compile_start = Instant::now();
        let warm_artifact = warm_runtime.compile(&wasm_bytes)?;
        let warm_compile_ms = warm_compile_start.elapsed().as_millis();
        (warm_runtime, warm_artifact, Some(warm_compile_ms))
    } else {
        let runtime = WasmRuntime::new_with_runtime_config(Some(&cold_config))?;
        (runtime, cold_artifact, None)
    };

    let rss_after_compile_bytes = read_proc_status_bytes("VmRSS");

    let config = AppConfig::default_for(AppId::new("wasmtime-load-probe", "v1"));
    let prepare_start = Instant::now();
    let prepared = runtime.prepare(&artifact_bytes, config)?;
    let prepare_ms = prepare_start.elapsed().as_millis();

    let sequential_spawn_start = Instant::now();
    for i in 0..options.sequential_spawns {
        let instance = prepared.spawn_instance(vec![], 20_000u16.saturating_add(i as u16), None)?;
        drop(instance);
    }
    let sequential_spawn_total_ms = sequential_spawn_start.elapsed().as_millis();
    let rss_after_sequential_bytes = read_proc_status_bytes("VmRSS");

    let peak_live_start = Instant::now();
    let mut live_instances = Vec::with_capacity(options.peak_live_instances as usize);
    for i in 0..options.peak_live_instances {
        let instance = prepared.spawn_instance(vec![], 30_000u16.saturating_add(i as u16), None)?;
        live_instances.push(instance);
    }
    let peak_live_spawn_ms = peak_live_start.elapsed().as_millis();
    let rss_after_peak_live_bytes = read_proc_status_bytes("VmRSS");
    drop(live_instances);

    let rss_high_water_bytes = read_proc_status_bytes("VmHWM");

    if let Some(path) = cache_root {
        let _ = fs::remove_dir_all(path);
    }

    Ok(ProbeResult {
        scenario: options.scenario,
        cache_enabled: options.scenario.cache_enabled(),
        pooling_enabled: options.scenario.pooling_enabled(),
        component_path: options.component_path.clone(),
        sequential_spawns: options.sequential_spawns,
        peak_live_instances: options.peak_live_instances,
        cold_compile_ms,
        warm_compile_ms,
        prepare_ms,
        sequential_spawn_total_ms,
        peak_live_spawn_ms,
        rss_before_bytes,
        rss_after_compile_bytes,
        rss_after_sequential_bytes,
        rss_after_peak_live_bytes,
        rss_high_water_bytes,
    })
}

fn print_result(result: &ProbeResult) {
    let sequential_spawn_avg_ms =
        result.sequential_spawn_total_ms as f64 / result.sequential_spawns as f64;

    println!("scenario={}", result.scenario.as_str());
    println!("component_path={}", result.component_path.display());
    println!("cache_enabled={}", result.cache_enabled);
    println!("pooling_enabled={}", result.pooling_enabled);
    println!("sequential_spawns={}", result.sequential_spawns);
    println!("peak_live_instances={}", result.peak_live_instances);
    println!("cold_compile_ms={}", result.cold_compile_ms);
    println!(
        "warm_compile_ms={}",
        result
            .warm_compile_ms
            .map(|v| v.to_string())
            .unwrap_or_default()
    );
    println!("prepare_ms={}", result.prepare_ms);
    println!(
        "sequential_spawn_total_ms={}",
        result.sequential_spawn_total_ms
    );
    println!("sequential_spawn_avg_ms={sequential_spawn_avg_ms:.3}");
    println!("peak_live_spawn_ms={}", result.peak_live_spawn_ms);
    println!(
        "rss_before_bytes={}",
        result
            .rss_before_bytes
            .map(|v| v.to_string())
            .unwrap_or_default()
    );
    println!(
        "rss_after_compile_bytes={}",
        result
            .rss_after_compile_bytes
            .map(|v| v.to_string())
            .unwrap_or_default()
    );
    println!(
        "rss_after_sequential_bytes={}",
        result
            .rss_after_sequential_bytes
            .map(|v| v.to_string())
            .unwrap_or_default()
    );
    println!(
        "rss_after_peak_live_bytes={}",
        result
            .rss_after_peak_live_bytes
            .map(|v| v.to_string())
            .unwrap_or_default()
    );
    println!(
        "rss_high_water_bytes={}",
        result
            .rss_high_water_bytes
            .map(|v| v.to_string())
            .unwrap_or_default()
    );
}

fn read_proc_status_bytes(field: &str) -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status
        .lines()
        .find(|line| line.starts_with(field) && line.contains(':'))?;
    let value_kib = line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())?;
    Some(value_kib * 1024)
}
