/// Documents where each policy capability is enforced today in the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyEnforcementLayer {
    WasmtimeNetworkToggle,
    WasmtimeSocketAddrCheck,
    WasiHttpTransport,
    WasiSocketEgress,
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
        capability: "p3_http_service",
        primary_layer: PolicyEnforcementLayer::WasiHttpTransport,
        authoritative_enforcement: true,
        authoritative_counters: false,
        notes: "P3 bounded HTTP handlers share P2 outgoing HTTP policy hooks and the dedicated identity thread. Bodies are capped at 16 MiB and execution at 30 seconds. P3 outbound TCP has destination/supervisor checks, owned connection slots, metered writes and host connect/read/write deadlines. Raw UDP/bind, socket options and native triggers are not supported; this is not full P3 conformance. Handler timeout/body-denial counters are not separately exported.",
    },
    PolicyBoundaryCapability {
        capability: "network_host_io_deadlines",
        primary_layer: PolicyEnforcementLayer::WasiSocketEgress,
        authoritative_enforcement: true,
        authoritative_counters: true,
        notes: "P2 raw connect/name resolution has a 10-second host deadline; pending raw TCP/UDP read/write waits, blocking streams, splice, and asynchronous stream destruction have 30-second host caps. Pending deadlines survive poll cancellation; idle listeners are untimed. Expiration traps the CLI invocation or HTTP request store and feeds per-instance timeout counters. Outgoing HTTP retains its bounded connect/first-byte/idle/total transport caps.",
    },
    PolicyBoundaryCapability {
        capability: "http_outbound",
        primary_layer: PolicyEnforcementLayer::WasiHttpTransport,
        authoritative_enforcement: true,
        authoritative_counters: true,
        notes: "WASI HTTP resolves once, applies TCP CIDR and supervisor checks, verifies TLS against the original host, and enforces host deadlines, response-body limits, connection reservations, and wire egress quotas. Explicit ExtendedLimits network ceilings intersect with policy ceilings.",
    },
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
        notes: "UDP address allow/deny is enforced through the runtime socket hook. Datagram payloads feed the shared egress quota; separate UDP operation counters are not wired.",
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
        capability: "socket_payload_egress_bytes",
        primary_layer: PolicyEnforcementLayer::WasiSocketEgress,
        authoritative_enforcement: true,
        authoritative_counters: true,
        notes: "P2 raw TCP payload reservations, accepted UDP payloads, and outgoing HTTP wire writes share the resolved egress quota. TCP charges remain on failure because Wasmtime can send a prefix or queue background writes. This measures guest socket payload commitments plus HTTP/TLS wire bytes, not total NIC traffic; DNS, packet overhead, and host-served incoming HTTP responses are excluded.",
    },
    PolicyBoundaryCapability {
        capability: "network_egress_bytes",
        primary_layer: PolicyEnforcementLayer::ExternalEbpf,
        authoritative_enforcement: false,
        authoritative_counters: false,
        notes: "The socket payload and outgoing HTTP budget is enforced, but total network traffic is not metered: packet overhead, retransmissions, host DNS, and host-served incoming HTTP responses are excluded.",
    },
];

pub fn current_policy_boundary() -> &'static [PolicyBoundaryCapability] {
    POLICY_BOUNDARY_CAPABILITIES
}
