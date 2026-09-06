# Step 46 — Production Hardening and Release Gates

## Goal

This document captures the deeper audit findings that are specifically about **hardening, operational safety, release quality, and production gates**.

Where Step 45 focuses on distributed control-plane correctness, this step focuses on whether the platform can be operated safely and predictably in a real environment.

---

## Audit framing

To call the platform base “production-ready”, the following must all be true:

- security guarantees are real, not just documented,
- admin and artifact surfaces fail closed,
- runtime policy is enforced where operators expect it,
- recovery behavior is conservative and auditable,
- release/upgrade workflows are trustworthy,
- CI actually blocks regressions in the critical paths.

Right now, the codebase is promising, but there are still several base-layer gaps that should be treated as **release blockers**.

---

## P0 — Critical / Release-blocking Hardening Findings

### P0-1. Outbound TCP policy is effectively bypassed for profiles that should disallow it

- [x] Separate inbound-listen permission from outbound-connect permission in the WASI runtime setup.
- [x] Fix `socket_addr_check` so external non-loopback destinations are not automatically allowed when policy forbids outbound TCP.
- [x] Add policy tests for profiles such as `StaticSite` that should not be able to make outbound TCP connections.

**Evidence**

- `crates/common/src/policy.rs:190-194` resolves `allow_inbound: true`.
- `crates/common/src/policy.rs:304-318` defines `StaticSite` with `allow_outbound_tcp = false`.
- `crates/runtime/src/executor.rs:137-142` enables TCP when either outbound TCP **or inbound** is allowed.
- `crates/supervisor/src/lib.rs:380-391` allows any non-loopback external destination.

**Why this matters**

This is a direct policy/enforcement mismatch. Operators can configure a profile that appears to disable outbound TCP while the runtime still allows it.

That is a real isolation bug, not a documentation gap.

**Validation after fix**

- [x] `crates/common/src/policy.rs` now carries `allow_inbound` as operator/profile-resolved policy state instead of hard-coding inbound allowance during resolution.
- [x] `PolicyProfile::BackgroundWorker` now resolves with inbound TCP bind denied, while `StaticSite` still permits binding but denies outbound TCP.
- [x] `crates/runtime/src/executor.rs` now always installs a policy-aware socket gate that evaluates bind vs connect separately before any supervisor-provided extra socket check runs.
- [x] `crates/runtime/src/tests.rs` now covers bind-vs-connect separation, CIDR enforcement, policy-first composed socket checks, `StaticSite` outbound denial, and `BackgroundWorker` bind denial.
- [x] WSL validation passed with `cargo test -p common -p runtime -p ctl --no-run`, plus targeted `common` and `runtime` policy tests.
- [ ] Full policy-aware WASI host wiring and authoritative per-socket accounting are still broader follow-up work under **P1-1**.

---

### P0-2. Artifact server is externally exposed and unauthenticated

- [x] Bind the artifact server to loopback by default.
- [x] Add authentication/authorization for any remote artifact transfer path.
- [x] Add an explicit advertised artifact endpoint separate from the local bind address.
- [x] Add tests proving remote unauthenticated clients cannot upload or fetch artifacts.

**Evidence**

- `crates/storage/src/artifact_server.rs:20-34` and `crates/storage/src/artifact_server.rs:43-76` expose unauthenticated GET/PUT routes.
- `crates/node/src/main.rs:2516-2525` binds the artifact server on `0.0.0.0`.

**Why this matters**

This is a supply-chain and persistence surface. Hash verification alone does not provide authorization.

**Validation after fix**

- [x] `crates/node/src/main.rs` now binds the artifact server on `127.0.0.1` by default, while separately supporting a configured advertised artifact endpoint for peers.
- [x] `crates/storage/src/artifact_server.rs` now rejects non-loopback peers unless they present an authorized bearer token.
- [x] `crates/node/src/main.rs` now binds the artifact server on `127.0.0.1` by default, supports a separate advertised artifact endpoint, and the current artifact plane now prefers signed transfer manifests for bootstrap and deploy fetch paths.
- [x] `crates/messaging/src/events.rs` and `crates/node/src/handlers.rs` now carry and use authenticated artifact transfer metadata for peer bootstrap and deploy fetch paths.
- [x] WSL validation passed with targeted `storage`, `messaging`, and `node` tests plus `cargo test -p storage -p messaging -p node -p ctl -p e2e --no-run`.
- [ ] The artifact plane still does not have its stronger long-term identity model implemented yet; Step 47 now recommends **signed short-lived transfer manifests** as the target design, with the current scoped bearer-token flow retained as the bridge.

---

### P0-3. `require_tls` for admin auth is a warning, not a guarantee

- [x] Make startup fail if admin auth is enabled with `require_tls=true` and no admin TLS endpoint is configured.
- [x] Remove the gap between documented security contract and actual startup behavior.
- [x] Add tests covering `enabled=true`, `require_tls=true`, and missing TLS config.

**Evidence**

- `crates/common/src/auth.rs:39-44` documents that the node should refuse startup when auth requires TLS but the admin API is on HTTP.
- `crates/node/src/main.rs:1321-1335` only logs warnings instead of enforcing that rule.

**Why this matters**

A production operator can believe bearer-token auth is TLS-protected when the process is still serving plaintext HTTP.

**Validation after fix**

- [x] `crates/node/src/main.rs` now detects admin TLS material and serves the admin API over HTTPS when configured.
- [x] Dedicated `admin.tls_cert` / `admin.tls_key` configuration is now supported, with fallback to shared proxy TLS material when dedicated admin certs are not set.
- [x] `proxy::auth_middleware::check_admin_tls_requirement(...)` now evaluates the real admin TLS configuration instead of a hard-coded `false` path.
- [x] `config/production.toml` now validates with structured auth, `require_tls = true`, and explicit admin TLS certificate paths.
- [x] WSL validation passed with `cargo test -p node --bin wasm-node --no-run`, targeted node/config TLS tests, and config validation runs for production/staging TOML.
- [x] `crates/node/src/main.rs` now has HTTPS listener coverage that verifies a real HTTPS request succeeds with a test certificate.
- [x] Additional failure-path coverage now verifies missing certificate files and invalid PEM contents are rejected before the admin TLS listener starts.
- [x] `crates/node/src/main.rs` now also covers the startup contract directly:
  - `auth.enabled = true` + `auth.require_tls = true` + no admin/proxy TLS material fails the node-side startup check
  - shared `proxy.tls_cert` / `proxy.tls_key` still satisfy the admin TLS requirement when dedicated admin TLS material is absent
- [x] WSL validation passed with:
  - `cargo test -p node --bin wasm-node test_startup_tls_requirement_rejects_auth_without_any_tls_material -- --nocapture`
  - `cargo test -p node --bin wasm-node test_startup_tls_requirement_accepts_proxy_tls_fallback -- --nocapture`

---

### P0-4. Cluster artifact advertisement uses `127.0.0.1`, which breaks real multi-node exchange

- [x] Add `advertised_host` / `advertised_artifact_url` configuration.
- [x] Reject loopback advertisement in cluster mode.
- [x] Add a two-node test proving peer artifact transfer uses a routable address.

**Evidence**

- `crates/node/src/main.rs:762-766` sets `artifact_server_url` to `http://127.0.0.1:<artifact_port>`.
- `crates/messaging/src/events.rs:79-89` defines that URL as the peer artifact endpoint in `NodeJoined`.
- `crates/node/src/handlers.rs:582-592` pushes artifacts to `peer_artifact_url`.

**Why this matters**

In a real cluster, peer nodes cannot use another host’s loopback address. This blocks correct shared-nothing bootstrap and artifact synchronization.

**Validation after fix**

- [x] `crates/common/src/config.rs` now supports `admin.advertised_host` and `admin.advertised_artifact_url`, with the full URL taking precedence over host-only advertisement.
- [x] `crates/config/src/lib.rs` now merges, parses, and validates those settings, rejecting explicit loopback advertisements in operator-specified routable config.
- [x] `crates/node/src/main.rs` now derives the advertised artifact endpoint from config instead of hard-coding `http://127.0.0.1:<artifact_port>`.
- [x] `crates/node/src/handlers.rs` now rejects loopback `peer_artifact_url` values when processing another node’s `NodeJoined` event, failing closed for detected remote cluster exchange.
- [x] `crates/node/tests/cluster_bootstrap.rs` now uses a non-loopback advertised artifact URL in the two-node bootstrap simulation and asserts that it is routable.
- [x] Production and staging example configs now document the new advertised artifact settings.
- [x] WSL validation passed with `cargo test -p common -p config -p node --no-run`, targeted `node` artifact URL tests, and `cargo test -p node --test cluster_bootstrap --no-run`.
- [x] The default remains loopback for local-only/same-host development, while authenticated remote peer artifact transfer is now implemented with scoped tokens; the remaining future hardening question is the Step 47 move to signed transfer manifests.

---

## P1 — High Hardening Findings

### P1-1. Runtime policy wrappers exist, but the runtime links plain WASI host functions instead of a policy-aware surface

- [x] Decide whether policy enforcement lives in wrapped WASI host calls, supervisor hooks, eBPF, or a combination — then make that boundary explicit and complete.
- [ ] If deeper in-process enforcement is still required, add custom policy-aware host/resource wrapping on top of Wasmtime for the capabilities that its existing hooks do not expose directly.
- [x] Add tests to prove counters and enforcement are both authoritative.

**Evidence**

- `crates/runtime/src/policy_wasi.rs:1-110` contains policy-aware helper hooks.
- `crates/runtime/src/executor.rs:193-200` links standard `wasmtime_wasi::p2::add_to_linker_sync`.
- `crates/runtime/src/executor.rs:172-179` explicitly notes missing `allowed_paths` preopen wiring.

**Why this matters**

The platform already contains the beginnings of a stronger policy layer, but some capabilities still depend on a combination of Wasmtime hooks, supervisor checks, and outer enforcement instead of fully wrapped host calls. The remaining risk is a gap between policy existing and policy being on the live path for every capability, not a need to replace Wasmtime itself.

**Validation after partial fix**

- [x] `crates/runtime/src/executor.rs` now wires `policy.filesystem.allowed_paths` into `WasiCtxBuilder::preopened_dir(...)` instead of leaving the filesystem policy path as a documented TODO.
- [x] Filesystem preopens now use read-only vs read-write permissions derived from the existing policy flags (`allow_file_create` / `allow_file_delete`).
- [x] `crates/common/src/policy.rs` now rejects misleading writable-filesystem configs at resolve time: host filesystem writes are opt-in only, `allowed_paths` must be absolute, and a positive write budget without an explicit writable path is rejected.
- [x] `crates/runtime/src/tests.rs` now proves that an allowed preopen path succeeds and a missing configured path fails instance spawn.
- [x] WSL validation passed with `cargo test -p runtime` after the filesystem preopen wiring was added.
- [x] `crates/runtime/src/policy_tracker.rs` now tracks a real `open_fds_peak` high-water mark instead of exporting the end-of-run live FD count as a fake peak.
- [x] `crates/runtime/src/executor.rs` now resets per-instance active resource counters on `RunningInstance` drop, so teardown does not leave stale active-connection / active-FD counts behind.
- [x] `crates/runtime/src/executor.rs` now routes the live TCP bind/connect `socket_addr_check` path through the shared `PolicyEnforcer` instance instead of a disconnected policy snapshot, so the same runtime decision path also updates deny/active/total counters.
- [x] `crates/supervisor/src/lib.rs` now exports each live instance's `PolicyEnforcer` denial counters and active gauges into the shared Prometheus `PolicyMetrics` sink, with delta-based flushes on health ticks and final flush on instance exit.
- [x] `crates/runtime/src/limits.rs` now feeds Wasmtime `ResourceLimiter` memory/table growth successes and denials into `PolicyEnforcer` counters, so `memory_and_table_growth` is counted authoritatively on the live path instead of being enforcement-only.
- [x] `crates/runtime/src/lib.rs` now exposes `current_policy_boundary()` / `POLICY_BOUNDARY_CAPABILITIES`, making the chosen combination model explicit in code:
  - Wasmtime socket hook for TCP bind/connect and UDP address checks
  - Wasmtime network toggle for coarse DNS
  - Wasmtime preopens for filesystem visibility
  - Wasmtime resource limiter for memory/table growth
  - supervisor extra socket gate for stricter namespace/local-service filtering
  - eBPF as the remaining outer layer for capabilities that still lack host-call hooks
- [x] `crates/runtime/POLICY_LIMITATIONS.md` now matches the current implementation instead of describing TCP CIDR filtering and preopens as still entirely missing.
- [x] `crates/runtime/src/tests.rs` now covers:
  - `ExecutionStats.io_stats` reading the authoritative counter values exported by the active runtime policy boundary
  - `RunningInstance` drop resetting active per-instance counters while preserving cumulative totals/high-water marks
  - TCP connect approval incrementing `PolicyEnforcer` counters through the actual composed socket gate
  - extra gate denial rolling back the reserved outbound slot instead of leaving a stale active count
  - TCP bind denial incrementing the `bind_denied_total` counter through the runtime socket gate
  - the explicit policy-boundary declaration itself, including which capabilities are and are not currently authoritative
  - `ResourceLimiter` memory/table growth updating authoritative current/peak/denied counters
- [x] WSL validation passed with:
  - `cargo test -p runtime test_execution_stats_export_authoritative_policy_counters -- --nocapture`
  - `cargo test -p runtime test_running_instance_drop_resets_active_policy_counters -- --nocapture`
  - `cargo test -p runtime test_record_fd_open_and_close -- --nocapture`
  - `cargo test -p runtime test_reset_active_counters_clears_only_live_resource_counts -- --nocapture`
- [x] Additional WSL validation passed with:
  - `cargo test -p runtime test_composed_socket_addr_check_records_policy_tracked_tcp_connects -- --nocapture`
  - `cargo test -p runtime test_composed_socket_addr_check_rolls_back_reserved_slot_when_extra_check_denies -- --nocapture`
  - `cargo test -p runtime test_composed_socket_addr_check_uses_policy_enforcer_bind_denial_counters -- --nocapture`
- [x] WSL validation passed for supervisor-side metrics export with:
  - `cargo test -p supervisor --lib -- --nocapture`
  - `cargo test -p node --bin wasm-node --no-run`
- [x] Additional WSL validation passed for authoritative resource-limiter counters with:
  - `cargo test -p runtime -- --nocapture`
- [x] Additional WSL validation passed for filesystem policy hardening with:
  - `cargo test -p common --lib -- --nocapture`
- [x] Boundary-declaration WSL validation passed with:
  - `cargo test -p runtime test_policy_boundary_declares_runtime_socket_gate_as_authoritative_for_tcp -- --nocapture`
  - `cargo test -p runtime test_policy_boundary_declares_remaining_non_authoritative_gaps_explicitly -- --nocapture`
- [ ] Full host/resource wrapping for the remaining capabilities is an optional larger follow-up item; the current step does **not** imply replacing Wasmtime, only possibly extending the host surface built on top of it where its existing hooks are insufficient. This is not a blocker for the current non-Windows production baseline.

---

### P1-2. Admin hardening defaults are not production-safe, and the production example still steers operators into legacy token mode

- [x] Add explicit bind-address configuration for admin/artifact listeners.
- [x] Default admin API to loopback bind in production templates.
- [x] Replace `admin.auth_token` in `config/production.toml` with the structured auth config.
- [x] Stop defaulting legacy-token mode to `require_tls = false` for anything resembling production guidance.

**Evidence**

- `config/production.toml:35-43` uses legacy `admin.auth_token`.
- `crates/common/src/auth.rs:267-277` converts legacy token mode into `require_tls: false`.
- `crates/common/src/config.rs:249-256` defaults auth disabled.
- `crates/node/src/main.rs:2506-2525` binds admin and artifact listeners to `0.0.0.0`.

**Why this matters**

A production example should bias operators toward the hardened path, not the compatibility path.

**Validation after partial fix**

- [x] `crates/node/src/main.rs` now binds both the admin API and artifact server to `127.0.0.1` by default.
- [x] `config/production.toml` and `config/staging.toml` now use the structured `[auth]` section instead of legacy `admin.auth_token` production guidance.
- [x] `crates/common/src/config.rs`, `crates/config/src/lib.rs`, and `crates/node/src/main.rs` now support operator-configurable `admin.bind_address` and `admin.artifact_bind_address`, including env/CLI overrides and validation.
- [x] `crates/common/src/auth.rs` now explicitly documents legacy token mode as a backward-compatibility/local-only path rather than a production guidance path.
- [x] WSL validation passed with `cargo test -p config -p node --no-run`, `cargo run --bin wasm-node -- --validate-config config/production.toml`, `cargo run --bin wasm-node -- --validate-config config/staging.toml`, and targeted bind-address/config tests.
- [x] The admin listener now supports HTTPS with dedicated `admin.tls_cert` / `admin.tls_key`, while still allowing fallback to shared proxy TLS material if dedicated admin certs are absent.

---

### P1-3. Storage recovery is too destructive: DB open failure triggers file deletion and recreation

- [x] Replace “delete and recreate” with quarantine + operator-visible failure by default.
- [x] Add an explicit recovery mode/config for destructive rebuilds.
- [x] Preserve corrupted files for forensic and recovery analysis.

**Evidence**

- `crates/node/src/main.rs:476-505` deletes the DB file and recreates storage after open failure.

**Why this matters**

This is too aggressive for production. A failed open should not automatically mean “destroy the local state and continue” unless an operator explicitly opted into that behavior.

**Validation after fix**

- [x] `crates/common/src/config.rs` now defines explicit `storage.open_failure_mode` and `storage.integrity_failure_mode` policies, defaulting to non-destructive quarantine behavior.
- [x] `crates/node/src/main.rs` now quarantines unreadable redb files and fails startup by default instead of silently deleting and recreating them.
- [x] Operators can explicitly opt into fresh-local-state recreation with `open_failure_mode = "quarantine_and_recreate"`.
- [x] `crates/node/src/recovery.rs` now quarantines corrupted DB files on critical integrity failure by default, and the admin rebuild endpoint now quarantines state instead of deleting it.
- [x] `config/production.toml` and `config/staging.toml` now document the non-destructive defaults and the explicit destructive override mode.
- [x] WSL validation passed with `cargo test -p common -p config -p node --no-run`, plus targeted `config` parsing/override tests and a `node` quarantine test.
- [ ] The explicit `delete_and_exit` integrity mode still exists for operators who intentionally treat local state as disposable; that remains a destructive opt-in path rather than the default.

---

### P1-4. Wasmtime production tuning is still minimal

- [x] Evaluate Wasmtime code caching for faster repeated startup/compilation.
- [x] Evaluate pooling allocator / instance pooling where compatible with the component model path.
- [x] Add epoch interruption or another mechanism for long-running/hung guest control if applicable.
- [x] Add explicit table/resource limits where currently unlimited.

**Evidence**

- `crates/runtime/src/compiler.rs:7-17` sets only fuel metering, opt level, and component model.
- `crates/runtime/src/limits.rs:54-76` enforces memory growth but allows table growth without limits.
- A grep over `crates/runtime/**/*.rs` found no use of pooling allocator, epoch interruption, or code cache configuration.

**Why this matters**

This is not necessarily a correctness bug, but it is a readiness gap. Production Wasmtime deployments usually need stronger tuning than just “fuel + optimize for speed”.

**Validation after partial fix**

- [x] `crates/common/src/types.rs` now includes `max_table_elements` in `ExtendedLimits` / `ExtendedLimitsConfig`, with a non-zero default cap.
- [x] `crates/runtime/src/limits.rs` now enforces table growth limits through the existing `ResourceLimiter` instead of allowing tables to grow without bound.
- [x] `crates/runtime/src/executor.rs` now feeds resolved extended limits into the runtime resource limiter during instance spawn.
- [x] `crates/runtime/src/compiler.rs`, `crates/runtime/src/lib.rs`, and `crates/runtime/src/limits.rs` now enable epoch interruption, configure store deadlines, and run a background engine epoch ticker for coarse-grained runaway guest interruption.
- [x] `crates/common/src/config.rs`, `crates/config/src/lib.rs`, and `crates/node/src/main.rs` now support an optional `runtime.cache_directory` that enables Wasmtime code caching when configured.
- [x] `crates/common/src/config.rs`, `crates/config/src/lib.rs`, `crates/node/src/main.rs`, and `crates/runtime/src/compiler.rs` now support an opt-in Wasmtime pooling allocator configuration for component instances.
- [x] Production and staging templates now document pooling allocator settings as an explicit benchmark-first tuning option rather than a default.
- [x] `crates/runtime/src/tests.rs` now covers default table caps, config merge behavior, table-limit enforcement, epoch interruption, runtime initialization with a cache directory, and runtime initialization with pooling enabled.
- [x] WSL validation passed with `cargo test -p runtime`, `cargo test -p config merge_priority`, `cargo test -p config validation_rejects_zero_pooling`, and config validation runs for production/staging TOML.
- [x] `crates/runtime/examples/wasmtime_load_probe.rs` and `scripts/run_wasmtime_load_review.sh` now provide a repeatable WSL/Linux load-review path using the real `hello-axum` component to compare:
  - cold vs warm compile latency
  - repeated instantiation latency
  - peak-live instance spawn latency
  - RSS after compile and after holding multiple live instances
- [x] WSL sustained-load review passed with `wsl bash -lc 'cd /mnt/d/dev/Wasm-Cloud-Platform && bash scripts/run_wasmtime_load_review.sh target/wasmtime-load-review'`.
- [x] Current review outcome: Wasmtime code cache is a clear win for repeated compilation, while pooling allocator did **not** improve instantiation throughput for the current component/workload shape, so production defaults remain:
  - code cache enabled where persistent local disk is available
  - pooling allocator disabled by default unless a deployment-specific rerun shows a material gain without unacceptable RSS growth

---

## P1 — Additional high-signal validation findings

### P1-5. Runtime entry-point lookup does not implement the documented fallback to top-level `run` / `_start`

- [x] Implement the actual fallback behavior promised by the runtime log/error message.
- [x] Add runtime tests for all supported entry-point styles: `wasi:cli/run@0.2.x#run`, top-level `run`, and `_start` if supported.
- [x] Re-baseline the runtime test suite after entry-point support is corrected.

**Evidence**

- `crates/runtime/src/executor.rs:252-335` only searches for `wasi:cli/run@0.2.x#run` before returning `export not found`.
- The error log at `crates/runtime/src/executor.rs:331-335` claims the runtime looked for `wasi:cli/run@0.2.x#run, run, or _start`, but the code shown above does not implement those fallbacks.
- WSL validation: `cargo test -p runtime` failed in `crates/runtime/src/tests.rs:113-123`, `157-167`, `194-204`, `227-241`, `265-273`, and `340-348` because the tests export top-level `run`, not the WASI interface form.

**Why this matters**

This is a real runtime correctness gap, not just a stale test. The implementation and the declared contract diverge.

**Validation after fix**

- [x] `crates/runtime/src/executor.rs` now resolves entry points in the documented order: `wasi:cli/run@0.2.x#run`, then top-level `run`, then top-level `_start`.
- [x] Entry-point invocation now supports both the WASI Preview 2 `result<(), ()>` shape and a no-result compatibility fallback before trying the untyped call path.
- [x] `crates/runtime/src/tests.rs` now covers `wasi:cli/run@0.2.6#run`, top-level `run`, and the configured fallback candidate list including `_start`.
- [x] WSL validation passed with `cargo test -p runtime`, re-baselining the previously failing runtime suite.

---

### P1-6. Runtime policy helpers still call deprecated split check/record methods that were explicitly replaced to avoid TOCTOU races

- [x] Replace split `check_*` + `record_*` calls in `policy_wasi.rs` with the atomic `check_and_record_*` helpers where applicable.
- [x] Re-run runtime tests after the policy helper update.
- [x] If these wrappers remain unused today, either wire them properly or remove misleading dead/half-ready policy plumbing.

**Evidence**

- `crates/runtime/src/policy_wasi.rs:41-52` still uses deprecated `check_egress()` and `record_egress()`.
- `crates/runtime/src/policy_wasi.rs:100-111` still uses deprecated `check_fs_write()` and `record_fs_write()`.
- `crates/runtime/src/policy_tracker.rs:198-239` and `342-382` show the deprecated APIs and their atomic replacements, with explicit notes that the old versions should not be used because of TOCTOU races.
- WSL compile/test validation emitted deprecation warnings for these calls.

**Why this matters**

This is exactly the kind of issue that signals the runtime hardening path is incomplete. Even if the wrappers are not fully wired yet, the validated build is already telling us the current implementation is using the wrong API shape.

**Validation after fix**

- [x] The atomic check-and-record behavior was preserved while the unused `policy_wasi` helper module was removed from the runtime crate surface instead of remaining as misleading half-ready plumbing.
- [x] The active runtime policy boundary is now explicit in code: `WasiCtxBuilder` configuration, `socket_addr_check`, `ResourceLimiter`, filesystem preopens, and direct `PolicyEnforcer` usage.
- [x] WSL validation passed with `cargo test -p runtime` after removing the unused wrapper module.

---

## P2 — Medium Hardening / Release Findings

### P2-1. CI security gates are weak and non-blocking

- [x] Add top-level workflow `permissions:` minimization.
- [x] Pin critical GitHub Actions and tool installs to immutable versions/SHAs.
- [x] Make dependency/security audit blocking for protected branches, or at least blocking for severe advisories.
- [x] Consider adding `cargo-deny` or equivalent policy checks.

**Evidence**

- `.github/workflows/ci.yml:1-11` had no visible workflow-level permission tightening.
- `.github/workflows/ci.yml:20-26` and other steps pin actions by tag (`@v4`, `@stable`) rather than immutable SHAs.
- `.github/workflows/ci.yml:221-237` marked security audit `continue-on-error: true` and installed `cargo-audit` ad hoc.
- Repo scan did **not** find `deny.toml`, `audit.toml`, or `.github/dependabot.yml`.

**Why this matters**

CI should not just be broad and convenient; it should also be hard to tamper with and hard to silently bypass.

**Validation after partial fix**

- [x] `.github/workflows/ci.yml` now sets top-level `permissions: contents: read` and adds workflow concurrency cancellation.
- [x] The security audit job is now blocking instead of `continue-on-error`, and runs `cargo audit --deny warnings`.
- [x] A new `cargo-deny` job now enforces advisory, bans, and source policy checks using a repository `deny.toml`.
- [x] `.github/dependabot.yml` now enables weekly updates for both Cargo dependencies and GitHub Actions.
- [x] WSL validation passed with `cargo metadata --locked --format-version 1 > /dev/null` and `git --no-pager diff --check`.
- [x] `.github/workflows/ci.yml` now pins `actions/checkout`, `actions/cache`, `actions/upload-artifact`, and `dtolnay/rust-toolchain` to immutable commit SHAs, and pins `cargo-audit` / `cargo-deny` install versions.

---

### P2-2. CI does not cover all critical integration suites already present in the repository

- [x] Add CI jobs for `crates/node/tests`, `crates/proxy/tests`, and `crates/supervisor/tests`.
- [x] Decide which `vm-testbed` tests are required pre-merge vs nightly.
- [x] Add a required “cluster bootstrap + gateway + graceful shutdown” verification set.

**Evidence**

- `.github/workflows/ci.yml:73-81` only runs selected integration suites (`storage`, `runtime`).
- `.github/workflows/ci.yml:127-131` runs `cargo test -p e2e`.
- Existing integration suites live under `crates/node/tests`, `crates/proxy/tests`, and `crates/supervisor/tests` but are not explicitly gated here.

**Why this matters**

The missing CI coverage aligns too closely with several of the most important production-readiness risks found in the audit.

**Validation after fix**

- [x] `.github/workflows/ci.yml` now has explicit pre-merge jobs for proxy gateway integration tests, supervisor integration tests, and node critical integration tests.
- [x] The node critical integration job now starts a JetStream-enabled NATS container and runs `cluster_bootstrap` plus `db_integration` explicitly.
- [x] The required “cluster bootstrap + gateway + graceful shutdown” verification set is now represented by dedicated CI coverage across `cluster_bootstrap`, `gateway_integration`, and `supervisor/tests/graceful_shutdown.rs`.
- [x] The E2E lane now also runs the live authoritative cluster-node registry regression (`test_live_cluster_registry_drives_artifact_authorize_audience_set`) so deploy fan-out and audience-bound artifact authorization are exercised against a real two-node fixture.
- [x] `.github/workflows/vm-testbed-nightly.yml` now moves Firecracker/KVM-dependent `vm-testbed` coverage to a scheduled/manual self-hosted workflow instead of treating it as a universal pre-merge gate.
- [x] WSL validation passed with targeted compile coverage for the newly gated suites and `git diff --check` on the workflow files.

---

### P2-3. Upgrade pipeline verifies hash integrity but not release provenance

- [x] Add signed release metadata or artifact signatures to upgrade events.
- [x] Stage upgrade binaries to a temp path and use atomic replace/rename.
- [x] Add rollback and crash-safe install semantics.

**Evidence**

- `crates/messaging/src/events.rs:106-122` defines `NodeUpgrade` with URL + SHA-256 only.
- `crates/node/src/upgrade.rs:13-52` verifies only SHA-256.
- `crates/node/src/upgrade.rs:61-86` writes the new binary directly to the target path.

**Why this matters**

A SHA-256 hash proves integrity of the downloaded bytes relative to the message, but not trust in the release source or issuer.

**Validation after fix**

- [x] `crates/messaging/src/events.rs` now supports an optional Ed25519 signature on `NodeUpgrade` metadata.
- [x] `crates/ctl/src/cmds/platform.rs` now supports signing upgrade metadata with an Ed25519 private key file before publishing the upgrade event.
- [x] `crates/common/src/config.rs`, `crates/config/src/lib.rs`, and `crates/node/src/main.rs` now support an optional `runtime.upgrade_signing_public_key` used to require and verify signed upgrade metadata.
- [x] `crates/node/src/upgrade.rs` now verifies upgrade signatures when a public key is configured, stages downloaded binaries to a temporary file, renames them into a hash-suffixed final path, and preserves `current` / `previous` rollback links.
- [x] `crates/node/src/handlers.rs` now rejects upgrade events whose signature verification fails before download/install proceeds.
- [x] WSL validation passed with `cargo test -p node upgrade`, `cargo test -p config test_validation_rejects_invalid_upgrade_signing_public_key`, and config validation runs for production/staging TOML.
- [x] `crates/common/src/upgrade_provenance.rs`, `crates/messaging/src/events.rs`, `crates/ctl/src/cmds/platform.rs`, and `crates/node/src/upgrade.rs` now support a stronger delegated provenance model: a root-trusted release-key delegation plus a delegated signed release manifest bound to the upgrade artifact hash, URL, protocol version, and binary version.
- [x] Additional WSL validation passed for delegated provenance verification with:
  - `cargo test -p common --lib -- --nocapture`
  - `cargo test -p node --bin wasm-node test_verify_upgrade_signature_accepts_release_provenance_bundle -- --nocapture`
  - `cargo test -p node --bin wasm-node test_verify_upgrade_signature_rejects_mismatched_release_provenance -- --nocapture`
- [ ] Transparency logs / external attestations remain deferred future hardening, but upgrade verification is no longer limited to a detached event signature.

---

### P2-4. Runtime test infrastructure has brittle and environment-dependent assumptions

- [x] Replace hardcoded absolute target paths in runtime tests with discovered workspace-relative paths or fixture generation.
- [x] Mark tests that require prebuilt Wasm fixtures as such, or build the fixture in the test/setup path.
- [x] Keep runtime correctness tests independent from a developer’s specific filesystem layout when possible.

**Evidence**

- `crates/runtime/src/tests.rs:31-36` reads `/mnt/d/dev/Wasm-Cloud-Platform/target/wasm32-wasip2/release/hello-axum.wasm` directly.
- WSL validation: `cargo test -p runtime` failed in `test_list_hello_axum_exports` because the file was not present.

**Why this matters**

This is a test-readiness issue rather than a runtime production bug, but it reduces confidence in automated validation and makes failures noisier than they need to be.

**Validation after fix**

- [x] `crates/runtime/src/tests.rs` no longer hardcodes an absolute developer-specific `/mnt/d/dev/...` fixture path and now discovers the built `hello-axum` component from the workspace target directory.
- [x] `crates/runtime/tests/runtime_integration.rs` now uses the same workspace-relative discovery strategy for real-component tests instead of hardcoded absolute target assumptions.
- [x] Real-component runtime tests now skip cleanly with an instructional message when the fixture is absent, rather than failing because of one developer’s filesystem layout.
- [x] WSL validation passed with `cargo test -p runtime`, including the real-component integration tests when the fixture was present.

---

### P2-5. Node cluster-bootstrap tests were not self-contained, and once NATS was provisioned they exposed the JetStream stream-definition bug

- [x] Decide whether these tests should self-provision NATS or be clearly marked/segregated as environment-dependent integration tests.
- [x] Align them with the containerized harness style used elsewhere in the repo if possible.
- [x] After making them self-contained, keep them as a regression suite for JetStream setup and bootstrap semantics.

**Evidence**

- `crates/node/tests/cluster_bootstrap.rs:30-34` hardcodes `nats://127.0.0.1:4222` and documents that the operator should run a NATS container manually.
- Initial WSL validation failed with `Connection refused (os error 111)` when no NATS server was running.
- After provisioning a real NATS+JetStream instance in WSL, `cargo test -p node --tests` still failed in `crates/node/tests/cluster_bootstrap.rs:55`, `124`, `198`, and `283` because `setup_jetstream()` hit the overlapping-subject error from `crates/messaging/src/lib.rs:192-201`.

**Why this matters**

There are two separate issues here:

1. the tests are not self-contained enough for reliable CI/developer validation,
2. once the environment is correct, they surface the real control-plane bug in JetStream stream creation.

**Validation after fix**

- [x] `crates/node/tests/cluster_bootstrap.rs` now self-provisions a private JetStream-enabled NATS container per test instead of assuming a manually started `nats://127.0.0.1:4222`.
- [x] The bootstrap suite now uses containerized runtime detection (`docker` or `podman`) and remains a direct regression suite for bootstrap/session and JetStream setup behavior.
- [x] `crates/node/tests/e2e_smoke.rs` is now explicitly marked as an ignored acceptance-style smoke test so `cargo test -p node --tests` reflects reliable default node test coverage rather than a long-running full-platform scenario.
- [x] WSL validation passed with `cargo test -p node --tests`.

---

### P2-6. Repository-level security/release governance files are missing

- [x] Add `SECURITY.md`.
- [x] Add dependency/update policy (`dependabot.yml` or equivalent).
- [x] Add a Rust supply-chain policy file (`deny.toml` or equivalent) if you plan to enforce dependency rules.
- [x] Add a pinned `rust-toolchain.toml` if reproducibility matters for production releases.

**Evidence**

- Repo scan found no `SECURITY.md`.
- Repo scan found no `.github/dependabot.yml`.
- Repo scan found no `deny.toml` or `audit.toml`.
- Repo scan found no `rust-toolchain.toml` / `rust-toolchain` file.

**Why this matters**

These are not runtime bugs, but they are standard pieces of a production release process.

**Validation after fix**

- [x] `SECURITY.md` now documents private reporting expectations and the high-risk platform areas that should be treated as security-sensitive.
- [x] `.github/dependabot.yml` is present and tracks both Cargo and GitHub Actions updates.
- [x] `deny.toml` is present and is already exercised by the CI dependency-policy job.
- [x] `rust-toolchain.toml` now pins the Rust toolchain, required components, and the `wasm32-wasip2` target for more reproducible builds.
- [x] WSL validation passed with targeted workflow/config checks and existing compile/test coverage after adding the governance files.
- [x] `.github/workflows/release.yml` now provides a pinned release workflow for tagged builds and manual release refs:
  - immutable GitHub Action SHAs
  - `cargo build --locked --frozen --release`
  - deterministic release staging and tarball packaging
  - `SHA256SUMS` plus `RELEASE-MANIFEST.json`
- [x] `scripts/create_release_manifest.sh` now records the git SHA, `Cargo.lock` hash, Rust/Cargo versions, and per-artifact hashes/sizes for the published release set.
- [x] P10-01 source remediation strengthens the release gate with a closed
  platform/eBPF artifact allowlist, clean-source and immutable-SHA enforcement,
  deterministic packaging, SPDX 2.3 generation, GitHub OIDC/Sigstore SLSA and
  SBOM attestations, CI pre-publication verification, and a fail-closed operator
  admission script. Manual runs are explicitly non-promotable candidates.
- [x] `scripts/test-release-supply-chain.sh` proves successful admission,
  byte-for-byte reproducible archives, and rejection after artifact tampering.
- [ ] A production tag must still run this workflow successfully and an
  independent operator must preserve and verify its real OIDC attestations;
  local tests cannot manufacture that release evidence.
- [x] Additional WSL validation passed with:
  - `wsl bash -lc 'bash -n scripts/create_release_manifest.sh'`
  - `wsl bash -lc 'cargo metadata --locked --format-version 1 > /dev/null'`

---

## Release gates

Use these as explicit go/no-go checks.

### Before calling the platform base “production-hardened”

- [x] No policy profile can accidentally make forbidden outbound TCP connections.
- [x] Admin auth + TLS contract is actually enforced at startup.
- [x] Artifact server is not remotely writable/readable without authorization.
- [x] Cluster nodes advertise routable artifact endpoints, not loopback.
- [x] DB open failures do not silently destroy state by default.
- [x] Route replay and control-plane subscriptions are fixed per Step 45.

### Before a beta release

- [x] `cargo check --workspace` passes after cleanup.
- [x] `cargo test --workspace --no-run` passes.
- [x] Critical integration suites are in CI.
- [x] Security/dependency audit policy is defined and enforced.
- [x] Production config templates use the hardened auth path.

### Before a GA release

- [x] Upgrade artifacts are provenance-protected, not just hash-checked.
- [x] Wasmtime tuning and resource caps are reviewed for sustained multi-tenant production load.
- [x] Recovery policy is operator-safe and preserves forensic evidence.
- [x] Release pipeline is reproducible and pinned.

---

## Validation checklist

### Runtime / policy validation

- [x] A `StaticSite`-like profile cannot open outbound TCP connections.
- [x] File access outside configured policy boundaries fails as expected.
- [x] Resource exhaustion tests confirm enforced table/memory/runtime limits.
- [x] Sustained-load review has a repeatable runtime benchmark path and a recorded recommendation for cache vs pooling defaults.

### Admin / artifact security validation

- [x] Admin startup fails when TLS is required but not configured.
- [x] Legacy production example no longer encourages insecure auth mode.
- [x] Artifact upload/download from an untrusted remote client is denied.

### Recovery / release validation

- [x] Simulated DB corruption does not auto-destroy evidence/state without explicit operator choice.
- [x] Upgrade flow uses signed or provenance-verified artifacts.
- [x] Upgrade install path is crash-safe and rollback-capable.
- [x] Release workflow uses pinned actions, `--locked --frozen` builds, and deterministic release packaging.

---

## Final recommendation for this step

Treat the items in this file as **base-platform release gates**, not polish.

If Step 45 is about making the distributed system *correct*, this step is about making the platform *safe to trust in production*.

The minimum hardening priority should now be treated as:

- [x] outbound policy enforcement bug
- [x] artifact server exposure
- [x] admin TLS failure-path coverage
- [x] routable artifact advertisement
- [x] non-destructive recovery behavior
- [x] stronger CI/release gates

## Phase 10 reconciliation

The supported platform source contract is now closed for producing a signed
release candidate. This does not approve a working-tree build for production.
The exact tag still needs workflow-produced provenance and independent
verification, and the deployment operator must qualify the selected hosts,
PKI, NATS topology, telemetry/paging path, secret-root integration, and resource
envelope. Firecracker, PostgreSQL, the OIDC Hub, Vault, HAProxy, and Prometheus
were validation fixtures or external integrations; they are not added to the
platform release by this hardening plan. See the
[Phase 10 readiness reconciliation](process/prod_validation/evidence/2026-08-29-single-host/PHASE_10_RECONCILIATION.md).
