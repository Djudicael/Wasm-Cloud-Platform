# Step 44 — Remaining Backlog: Supporting Findings

This file now tracks **only the unresolved supporting backlog** from the platform audit.

It intentionally omits historical narrative, already-completed hardening work, and deep rationale already captured in:

- `INFRA_IMPL/42_PLATFORM_CODE_AUDIT_SUMMARY.md`
- `INFRA_IMPL/43_PLATFORM_CODE_AUDIT_CORE_FINDINGS.md`
- `INFRA_IMPL/46_PRODUCTION_HARDENING_AND_RELEASE_GATES.md`
- `INFRA_IMPL/47_ARTIFACT_PLANE_IDENTITY_MODEL_ADR.md`

---

## P2 — Medium backlog

### P2-1. Fix proxy path-prefix stripping with query strings

**Remaining work**

- [x] Rewrite prefix stripping to operate on the path and query independently.
- [x] Add regression coverage for `strip_prefix=true` with query parameters.
- [x] Cover: no query, single query, multi-param query, and empty stripped path.

**Refs**

- `crates/proxy/src/service.rs:464-480`

**Validation after fix**

- [x] `crates/proxy/src/service.rs` now strips prefixes against `uri.path()` and reattaches the untouched query string instead of operating on the full URI string.
- [x] `crates/proxy/src/service.rs` now includes regression coverage for no-query, single-query, multi-query, empty-stripped-path, and non-matching-prefix cases.
- [x] WSL validation passed:
  - `cargo test -p proxy strip_uri_prefix_handles_path_and_query_independently -- --nocapture`
  - `cargo test -p proxy --lib -- --nocapture`

---

### P2-2. Replace Keycloak-specific JWKS lookup with real OIDC discovery

**Remaining work**

- [x] Fetch `/.well-known/openid-configuration` and use the discovered `jwks_uri`.
- [x] Preserve caching/refresh behavior without hard-coding a Keycloak path.
- [x] Add compatibility coverage against at least one non-Keycloak OIDC mock.

**Refs**

- `crates/proxy/src/gateway/oidc.rs:60-70`

**Validation after fix**

- [x] `crates/proxy/src/gateway/oidc.rs` now resolves JWKS through `/.well-known/openid-configuration` and uses the discovered `jwks_uri` instead of assuming a Keycloak-specific certs path.
- [x] The existing refresh loop and stale-cache refresh behavior are preserved; only the JWKS endpoint resolution path changed.
- [x] `crates/proxy/src/gateway/oidc.rs` now includes a mock issuer test whose discovered `jwks_uri` intentionally lives outside `/protocol/openid-connect/certs`, proving non-Keycloak compatibility.
- [x] WSL validation passed:
  - `cargo test -p proxy test_refresh_jwks_uses_discovered_jwks_uri -- --nocapture`
  - `cargo test -p proxy gateway::oidc -- --nocapture`

---

### P2-3. Align NATS metrics watcher with actual JetStream streams

**Remaining work**

- [x] Replace the hard-coded `WASM_PLATFORM` lookup with real platform stream aggregation or configurable stream selection.
- [x] Decide whether the metric reports one stream, a configured set, or total JetStream usage.
- [x] Add a test proving `setup_jetstream()` and the metrics watcher agree on stream names.

**Refs**

- `crates/metrics/src/nats.rs:112-119`
- `crates/messaging/src/lib.rs:121-202`

**Validation after fix**

- [x] `crates/metrics/src/nats.rs` now monitors the real platform stream set (`DEPLOY`, `CONTROL`, `NODE`, `HEALTH`, `PLATFORM`, `EBPF`) and exports per-stream byte/message gauges.
- [x] `crates/metrics/src/nats.rs` now includes a stream-name agreement test so the watcher stays aligned with `setup_jetstream()`.
- [x] WSL validation passed:
  - `cargo test -p metrics -- --nocapture`

---

### P2-4. Align rate-limit defaults with the documented shared-nothing model

**Remaining work**

- [x] Pick one canonical default for `RouteRateLimit.distributed` across CLI, types, and docs.
- [x] If shared-nothing remains the intended default, make node-local the default and require explicit opt-in for distributed mode.
- [x] Add CLI coverage proving `deploy` and `gateway set-rate-limit` converge on the same default behavior.

**Refs**

- `crates/ctl/src/cmds/deploy.rs:144-148`
- `crates/ctl/src/cmds/deploy.rs:369-373`
- `crates/ctl/src/cmds/gateway.rs:37-45`
- `crates/ctl/src/cmds/gateway.rs:94-102`
- `crates/common/src/types.rs:424-439`
- `INFRA_IMPL/24_RATE_LIMITING.md:57-59`

**Validation after fix**

- [x] `crates/common/src/types.rs` now defaults `RouteRateLimit.distributed` to `false`, making node-local limiting the canonical fallback when manifests or stored configs omit the field.
- [x] `crates/ctl/src/cmds/gateway.rs` now treats `--distributed` as an explicit opt-in flag instead of defaulting `gateway set-rate-limit` to distributed mode.
- [x] `crates/ctl/src/cmds/deploy.rs` keeps `--gateway-rps-distributed` as the explicit opt-in path and now has CLI parsing tests proving the default remains node-local.
- [x] `crates/ctl/src/cmds/manifest.rs` now tests that a manifest with `[gateway.rate_limit]` but no `distributed` field deserializes to node-local behavior.
- [x] WSL validation passed:
  - `cargo test -p ctl -- --nocapture`

---

### P2-5. Wire the namespace map into eBPF fallback mode

**Remaining work**

- [x] Call `dispatcher.set_namespace_map(namespace_map.clone())` in the fallback path as well.
- [x] Add fallback-mode coverage proving `TidConnection` and `TidDisconnection` still update port bindings.

**Refs**

- `crates/ebpf-monitor/src/lib.rs:308-312`
- `crates/ebpf-monitor/src/lib.rs:363-375`
- `crates/ebpf-monitor/src/actions.rs:737-752`

**Validation after fix**

- [x] `crates/ebpf-monitor/src/lib.rs` now wires the fallback `NamespaceMap` into the shared `ActionDispatcher`, matching the eBPF path.
- [x] `crates/ebpf-monitor/src/lib.rs` now includes fallback-mode coverage proving `TidConnection` binds a source port and `TidDisconnection` removes it through the dispatcher path.
- [x] WSL validation passed:
  - `cargo test -p ebpf-monitor test_fallback_dispatcher_updates_namespace_map_port_bindings -- --nocapture`
  - `cargo test -p ebpf-monitor --lib -- --nocapture`

---

### P2-6. Narrow the E2E “NATS partition” chaos blast radius

**Remaining work**

- [x] Scope packet loss to the actual NATS flow instead of all loopback traffic.
- [x] If loopback-wide loss is kept, rename the scenario so it does not claim to be NATS-only.
- [x] Add assertions proving admin, proxy, and artifact flows stay healthy during the intended failure mode.

**Refs**

- `crates/e2e/src/injector.rs:268-290`

**Validation after fix**

- [x] `crates/e2e/src/injector.rs` now prefers a scoped `iptables` DROP rule for the target NATS `ip:port` and falls back to a scoped `tc clsact` egress filter instead of loopback-wide `tc netem` packet loss.
- [x] `crates/e2e/src/chaos/l5_nats_partition.rs` now explicitly asserts that admin route reads, proxy traffic, and artifact uploads still work while the node's NATS connection is partitioned.
- [x] WSL validation passed:
  - `cargo test -p e2e --lib -- --nocapture`
  - `cargo test -p e2e --test chaos --no-run`

---

### P2-7. Remove E2E environment and host-helper footguns

**Remaining work**

- [x] Make preflight require the same container runtime policy the harness actually uses.
- [x] Stop using shell-interpolated `echo ... >> /etc/hosts` patterns under WSL.
- [x] Prefer direct file append via a controlled elevated helper path.

**Refs**

- `crates/e2e/src/chaos/mod.rs:162-177`
- `crates/e2e/src/fixture.rs:425-455`
- `crates/e2e/src/helpers.rs:449-483`

**Validation after fix**

- [x] `crates/e2e/src/fixture.rs` now centralizes host-runtime selection for Podman vs Docker, and `crates/e2e/src/chaos/mod.rs` reuses that policy for preflight instead of duplicating ad hoc checks.
- [x] `crates/e2e/src/helpers.rs` now validates `/etc/hosts` hostnames and uses stdin-driven `tee`-style elevated writes (`sudo`, `wsl -u root`, `pkexec`) instead of shell-interpolated append commands.
- [x] `crates/e2e/src/helpers.rs` and `crates/e2e/src/fixture.rs` now include unit coverage for hostname validation and runtime-selection behavior.
- [x] WSL validation passed:
  - `cargo test -p e2e --lib -- --nocapture`
  - `cargo test -p e2e --test cluster_registry -- --ignored --nocapture`

---

### P2-8. Replace string-based NAT subnet derivation in `vm-testbed`

**Remaining work**

- [x] Replace `.trim_end_matches(".1")` style logic with real IP/subnet parsing.
- [x] Add unit coverage for valid and invalid subnet derivation cases.

**Refs**

- `crates/vm-testbed/src/network.rs:274-279`

**Validation after fix**

- [x] `crates/vm-testbed/src/network.rs` now derives NAT subnets from parsed IPv4 CIDRs instead of trimming string suffixes from the bridge gateway address.
- [x] `crates/vm-testbed/src/network.rs` now validates invalid subnet inputs explicitly and covers both valid and invalid derivation cases in unit tests.
- [x] WSL validation passed:
  - `cargo test -p vm-testbed --lib -- --nocapture`

---

### P2-9. Bring example apps to realistic Wasm/native parity

**Remaining work**

- [x] Make all example apps honor `PORT` consistently in both native and Wasm execution.
- [x] Replace the `postgres-app` Wasm infinite-loop stub with a real Wasm-compatible implementation or explicitly mark the example unsupported for Wasm.
- [x] Document which examples are production-like versus smoke-test-only.

**Refs**

- `apps/hello-axum/src/main.rs:6-11`
- `apps/hello-axum/src/main.rs:228-233`
- `apps/echo-service/src/main.rs:5-10`
- `apps/echo-service/src/main.rs:71-76`
- `apps/postgres-app/src/main.rs:1-18`
- `apps/postgres-app/src/main.rs:100-116`

**Validation after fix**

- [x] `apps/hello-axum/src/main.rs` and `apps/echo-service/src/main.rs` now honor `PORT` on both native and Wasm paths instead of hardcoding the native listener port.
- [x] `apps/postgres-app/src/main.rs` now exposes a real Wasm-compatible TCP/HTTP implementation with the same `/`, `/health`, and `/query` surface as the native sample instead of an infinite-loop stub.
- [x] `apps/README.md` now documents which examples are production-like versus smoke-test-oriented.
- [x] WSL validation passed:
  - `cargo check -p hello-axum -p echo-service -p postgres-app`
  - `cargo check -p hello-axum -p echo-service -p postgres-app --target wasm32-wasip2`

---

### P2-10. Finish workspace build hygiene cleanup

**Remaining work**

- [x] Move the effective release-profile configuration to the workspace root `Cargo.toml`.
- [x] Capture a clean `cargo check --workspace` baseline in WSL.
- [x] Capture `cargo test --workspace --no-run` in WSL.
- [x] If feasible, add at least one targeted `cargo clippy` pass for core crates.

**Refs**

- `apps/echo-service/Cargo.toml:13-16`
- `apps/hello-axum/Cargo.toml:16-20`
- `apps/postgres-app/Cargo.toml:13-16`

**Validation after fix**

- [x] The ignored per-app `[profile.release]` sections were removed from `apps/hello-axum/Cargo.toml`, `apps/echo-service/Cargo.toml`, and `apps/postgres-app/Cargo.toml`, and the effective profile now lives in the workspace root `Cargo.toml`.
- [x] WSL validation passed:
  - `cargo check --workspace`
  - `cargo clippy -p common -p proxy --no-deps -- -D warnings`
  - `cargo test -p common --lib -- --nocapture`
  - `cargo test -p proxy --lib -- --nocapture`

---

## P3 — Low backlog

### P3-1. Make logging failure semantics match the message

**Remaining work**

- [x] Either actually fall back to stdout or change the log message to reflect fail-fast exit behavior.
- [x] Add a unit test covering log-file open failure semantics.

**Refs**

- `crates/common/src/logging.rs:442-448`

**Validation after fix**

- [x] `crates/common/src/logging.rs` now routes file-writer creation through a testable helper and emits a fail-fast message that matches the actual behavior (`...; exiting`) instead of claiming a stdout fallback.
- [x] `crates/common/src/logging.rs` now includes unit coverage for both successful file-writer creation and file-open failure semantics.
- [x] WSL validation passed:
  - `cargo test -p common logging -- --nocapture`
  - `cargo test -p common --lib -- --nocapture`

---

## Suggested validation backlog

### Build and static verification

- [x] `cargo check --workspace` (WSL)
- [x] `cargo test --workspace --no-run` (WSL)
- [x] `cargo test -p storage -p secrets -p messaging -p metrics` (WSL)
- [x] `cargo test -p supervisor -p runtime -p proxy -p internal_gateway` (WSL)

### Security and control-plane verification

- [x] Secret update roundtrip test across `ctl -> NATS -> node -> SecretProvider`
- [x] Admin API bind/auth/TLS matrix test
- [x] Artifact server unauthorized access test
- [x] Internal header forgery test for east-west traffic

### Recovery and shared-nothing verification

- [x] Route replay from JetStream history on empty local state
- [x] Full node restart with billing sequence continuity
- [x] Multi-node artifact advertisement/fetch test using non-loopback addresses
- [x] Metrics minute-boundary aggregation test

### Runtime isolation verification

- [x] Filesystem allowlist test once full WASI preopen enforcement is complete
- [x] Outbound network policy test for allowed vs denied destinations
- [x] Direct instance port reachability test in hardened mode

### Tooling and chaos verification

- [x] E2E NATS partition test that isolates NATS without blackholing unrelated localhost traffic
- [x] Docker/Podman preflight conformance test
- [x] `vm-testbed` subnet derivation unit test
- [x] Example-app parity check for native/Wasm port handling

