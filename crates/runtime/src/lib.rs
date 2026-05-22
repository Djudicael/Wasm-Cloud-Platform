pub mod compiler;
pub mod executor;
pub mod limits;
pub mod policy_tracker;
pub mod virtual_dns;

#[cfg(test)]
mod tests;

use common::{config::RuntimeSection, error::PlatformError, types::AppConfig};
use executor::PreparedModule;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::Engine;

const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyEnforcementLayer {
    WasmtimeNetworkToggle,
    WasmtimeSocketAddrCheck,
    WasmtimePreopenDirectories,
    WasmtimeResourceLimiter,
    SupervisorExtraSocketGate,
    ExternalEbpf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyBoundaryCapability {
    pub capability: &'static str,
    pub primary_layer: PolicyEnforcementLayer,
    pub authoritative_enforcement: bool,
    pub authoritative_counters: bool,
    pub notes: &'static str,
}

pub const POLICY_BOUNDARY_CAPABILITIES: &[PolicyBoundaryCapability] = &[
    PolicyBoundaryCapability {
        capability: "tcp_bind",
        primary_layer: PolicyEnforcementLayer::WasmtimeSocketAddrCheck,
        authoritative_enforcement: true,
        authoritative_counters: true,
        notes: "TCP bind allow_inbound and allowed_bind_ports are enforced through the runtime socket hook; supervisor may add stricter per-instance bind checks.",
    },
    PolicyBoundaryCapability {
        capability: "tcp_connect",
        primary_layer: PolicyEnforcementLayer::WasmtimeSocketAddrCheck,
        authoritative_enforcement: true,
        authoritative_counters: true,
        notes: "TCP connect allow/deny, CIDR filtering, and outbound connection counters flow through the runtime socket hook before the supervisor extra gate.",
    },
    PolicyBoundaryCapability {
        capability: "udp_socket_address",
        primary_layer: PolicyEnforcementLayer::WasmtimeSocketAddrCheck,
        authoritative_enforcement: true,
        authoritative_counters: false,
        notes: "UDP address allow/deny is enforced through the runtime socket hook, but UDP-specific byte/operation counters are not yet wired into PolicyEnforcer.",
    },
    PolicyBoundaryCapability {
        capability: "dns_lookup",
        primary_layer: PolicyEnforcementLayer::WasmtimeNetworkToggle,
        authoritative_enforcement: true,
        authoritative_counters: false,
        notes: "DNS is controlled only by coarse allow_ip_name_lookup on/off today; hostname-level policy and authoritative DNS counters are not wired.",
    },
    PolicyBoundaryCapability {
        capability: "filesystem_preopens",
        primary_layer: PolicyEnforcementLayer::WasmtimePreopenDirectories,
        authoritative_enforcement: true,
        authoritative_counters: false,
        notes: "Configured allowed_paths are enforced through preopened directories, but per-open/read/write activity is not yet captured from wrapped WASI host calls.",
    },
    PolicyBoundaryCapability {
        capability: "memory_and_table_growth",
        primary_layer: PolicyEnforcementLayer::WasmtimeResourceLimiter,
        authoritative_enforcement: true,
        authoritative_counters: true,
        notes: "Memory and table growth are enforced by Wasmtime ResourceLimiter and now feed PolicyEnforcer counters for current usage, peaks, and denied growth requests.",
    },
    PolicyBoundaryCapability {
        capability: "filesystem_write_bytes",
        primary_layer: PolicyEnforcementLayer::ExternalEbpf,
        authoritative_enforcement: false,
        authoritative_counters: false,
        notes: "Filesystem write byte limits exist in PolicyEnforcer but are not driven by wrapped WASI write hooks yet; authoritative enforcement still requires future host wrapping or external observation.",
    },
    PolicyBoundaryCapability {
        capability: "network_egress_bytes",
        primary_layer: PolicyEnforcementLayer::ExternalEbpf,
        authoritative_enforcement: false,
        authoritative_counters: false,
        notes: "Egress byte limits exist in PolicyEnforcer but are not driven by per-write TCP hooks yet; authoritative enforcement still requires future host wrapping or external observation.",
    },
];

pub fn current_policy_boundary() -> &'static [PolicyBoundaryCapability] {
    POLICY_BOUNDARY_CAPABILITIES
}

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
