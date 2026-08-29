# Start the Podman socket (Linux)
systemctl --user start podman.socket

# Point testcontainers to the Podman socket
export DOCKER_HOST=unix:///run/user/$(id -u)/podman/podman.sock

# Run the tests
cargo test -p messaging


wsl bash -c "cd /mnt/d/dev/Wasm-Cloud-Platform && DOCKER_HOST=unix:///run/user/1000/podman/podman.sock TESTCONTAINERS_RYUK_DISABLED=true /home/djudicael/.cargo/bin/cargo test -p messaging --no-fail-fast 2>&1 | tail -50"

DOCKER_HOST=unix:///run/user/1000/podman/podman.sock TESTCONTAINERS_RYUK_DISABLED=true


I want to build apps\hello-axum for wasi2p the actual code doesn workl but i saw it possible to make it work with axum base on https://github.com/twitchax/leptos-axum-wasi/blob/main/src/server.rs and https://github.com/bytecodealliance/wasi-rs/blob/main/crates/wasip2/examples/http-proxy.rs

i am building the project on wsl but the process is determined in the crate e2e harness because to test my platform i need no execute at least this code cargo test -p e2e test_deploy_and_serve_http -- --ignored --nocapture 2>&1 | tee e2e-test.log


https://github.com/bytecodealliance/wasmtime

https://github.com/bytecodealliance/wasi-rs/tree/main

https://bytecodealliance.org/

https://github.com/twitchax/leptos-axum-wasi/tree/main

https://docs.wasmtime.dev/introduction.html


Still In Progress / TODO:
- Source app attribution in internal gateway is unreliable (ephemeral source ports not tracked). The port_to_app map only tracks bind ports. This is a known limitation; the spec recommends a port-per-app approach.
- Cross-namespace e2e test not yet written.


What's NOT Implemented (Phase 3+)

Per the design doc, Phase 3 (SK_MSG kernel-level enforcement) and Phase 4 (security hardening: connection table TTL, full audit logging, rate limiting per source app in gateway, eBPF map access control) are left as follow-up work. Those require:
- `sockops` + `sk_msg` eBPF programs (Linux 5.8+)
- `MONITORED_SOCKETS` SockHash map
- Kernel capability detection at startup

The current implementation provides **synchronous in-process identity resolution** with **gateway-level namespace enforcement** — the core MVP described in the design document.

Phase 3: SK_MSG Enforcement — **NOT IMPLEMENTED** ❌
This requires Linux 5.8+ `sockops` + `sk_msg` eBPF programs. The spec marks this as optional ("Linux 5.8+").


 strace or gdb

test_query_params_with_postgres


Key Design Decisions

1. **Firecracker** over QEMU: 125ms boot time vs. 1-5s
2. **Alpine Linux** rootfs: ~5MB base, musl libc, openrc init
3. **Bridge + TAP** networking: Enables real L5 chaos tests with iptables
4. **Reuse existing E2E tests**: Same test logic, just swap `NodeProcess` for `MicroVm`
5. **Tiered testing strategy**: Fast native tests on every PR, slow VM tests nightly

The code is ready to use. All you need is a WSL2 terminal with KVM enabled.

What This Enables vs. Native E2E Tests

| Test Scenario | Native Process | MicroVM |
|---|---|---|
| **eBPF on kernel 5.8 vs 6.x** | Only host kernel | ✅ Test both kernels |
| **eBPF loading without `CAP_SYS_ADMIN`** | Always have caps | ✅ Real capability denial |
| **Redb corruption on power loss** | `kill -9` is graceful-ish | ✅ VM kill = real power loss |
| **Network partition via `tc netem`** | iptables on host | ✅ Isolated VM network |
| **Memory pressure / OOM** | Host OOM killer | ✅ VM-level OOM |
| **systemd notify integration** | Not testable | ✅ Real `sd_notify` |
| **Kernel panic recovery** | Not possible | ✅ VM reboots, node restores


# 1. Open WSL terminal
wsl

# 2. Navigate to project
cd /mnt/d/dev/Wasm-Cloud-Platform

# 3. Install Firecracker
./scripts/vm/install-firecracker.sh

# 4. Build VM images (one-time, ~10-15 minutes)
./scripts/vm/build-all-images.sh

# 5. Run single-node test
sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture

# 6. Run chaos tests
sudo cargo test -p vm-testbed --test vm_chaos -- --nocapture --test-threads=1

# 7. Or use the CLI to spawn a cluster
cargo run --bin vm-testbed-cli -- spawn-cluster --nodes 3



server streaming
  - client streaming
  - bidi streaming
  - trailer/error propagation under failure paths



  'toolchain' is a required input

  [gateway.auth]
  policy = "authenticated"
  
  [[gateway.endpoints]]
  path = "/health"
  methods = ["GET"]
  auth = "none"
  
  [[gateway.endpoints]]
  path = "/api/users"
  methods = ["GET"]
  auth = "authenticated"
  required_scopes = ["read:users"]
  
  [[gateway.endpoints]]
  path = "/api/users"
  methods = ["POST"]
  auth = "roles"
  allowed_roles = ["admin", "editor"]
  required_scopes = ["write:users"]
  
  [[gateway.endpoints]]
  path = "/api/admin"
  methods = ["POST", "DELETE"]
  auth = "roles"
  allowed_roles = ["admin"]
  required_scopes = ["admin:users"]
  
  [[gateway.endpoints]]
  path = "/api/public"
  methods = ["GET"]
  auth = "api_key"


 Sigstore/Cosign integration

 Official references I aligned this with:
 - Sigstore Rust client docs: https://docs.sigstore.dev/language_clients/rust/
 - `sigstore` crate docs: https://docs.rs/sigstore/latest/sigstore/
 - `sigstore-verify` crate docs: https://docs.rs/sigstore-verify/latest/sigstore_verify/
 - Cosign blob verification docs: https://docs.sigstore.dev/cosign/verifying/verify/
 - What it does not cover yet:
 - OTLP export end-to-end against a collector
 - negative/failure-path E2E for Vault auth/rejection
 
 The next sensible addition is an OTLP smoke test with a mock collector if you want coverage for the telemetry migration too.


 Email:    admin@example.com
2026-08-23T11:45:50.738865Z  INFO oidc_wasm_dev:     Password: Admin123