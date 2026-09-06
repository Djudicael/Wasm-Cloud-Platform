# Step 42 — Platform Code Audit Summary

## Goal

A fresh implementation audit of the current workspace with emphasis on:

- correctness and reliability gaps,
- security posture,
- alignment with the project’s **shared-nothing** architecture,
- WASI Preview 2 runtime safety boundaries,
- operability of the control plane, admin plane, and test/tooling surface.

This audit is meant to be a **working remediation guide**, not just a review memo.
Every major item below includes checkboxes so you can track implementation work.

---

## Scope Reviewed

### Workspace and docs

- [x] Root workspace manifest and build behavior
- [x] Root README and selected INFRA_IMPL documents relevant to runtime, rate limiting, configuration, and recovery
- [x] Existing implementation notes under `INFRA_IMPL/`

### Core crates reviewed

- [x] `crates/node`
- [x] `crates/supervisor`
- [x] `crates/runtime`
- [x] `crates/proxy`
- [x] `crates/internal_gateway`
- [x] `crates/storage`
- [x] `crates/messaging`
- [x] `crates/secrets`
- [x] `crates/config`
- [x] `crates/common`
- [x] `crates/metrics`
- [x] `crates/billing`

### Tooling and supporting crates reviewed

- [x] `crates/ctl`
- [x] `crates/e2e`
- [x] `crates/ebpf-monitor`
- [x] `crates/vm-testbed`
- [x] `apps/hello-axum`
- [x] `apps/echo-service`
- [x] `apps/postgres-app`

---

## Methodology

- [x] Static source review of crate entry points and critical paths
- [x] Review of configuration defaults and runtime bindings
- [x] Review of storage, messaging, secret-handling, and recovery flows
- [x] Review of TODO/FIXME/HACK markers and risky constructs
- [x] Workspace-level `cargo check --workspace` attempt
- [x] Project diagnostics scan
- [x] Parallel subsystem reviews to cross-check findings

### Audit constraints observed during review

- [x] `diagnostics` returned no editor/analysis errors.
- [x] Initial native Windows `cargo check --workspace` failed because the working volume ran out of disk space (`os error 112`).
- [x] After cleanup, WSL validation was run for the workspace and critical crates.
- [x] Build/test runs surfaced workspace warnings that per-app `[profile.release]` settings inside workspace members are ignored unless moved to the workspace root.

### WSL validation snapshot

- [x] `cargo check --workspace` in WSL completed
- [x] `cargo test -p storage -p messaging -p runtime -p proxy -p supervisor -p node --no-run` in WSL completed
- [x] `cargo test -p storage` passed in WSL
- [x] `cargo test -p proxy -p supervisor --tests` passed in WSL
- [x] `cargo test -p messaging` passed in WSL
- [x] `cargo test -p runtime` passed in WSL
- [x] `cargo test -p node --tests` passed in WSL
- [x] `cargo test -p node --test cluster_bootstrap` passed in WSL after fixing JetStream subject overlap
- [x] `cargo test -p node --bin wasm-node` passed in WSL after the secret transport / KEK sealing / bootstrap / shutdown hardening pass
- [x] `cargo test -p secrets -p messaging -p internal_gateway -p supervisor` passed in WSL after the same hardening pass
- [x] `cargo test -p e2e --test cluster_registry -- --ignored --nocapture` passed in WSL for the authoritative cluster-node registry / deploy fan-out regression
- [x] `cargo test --workspace --no-run` passed in WSL

The current audit is therefore no longer only a source review — it is now a **source + targeted WSL validation audit**. The remaining unchecked items are mostly additional regression coverage, full-workspace validation, or future-strengthening work captured in Steps 45–47.

---

## Executive Summary

The platform started this audit with several real security and distributed-correctness gaps, but the major base-platform items have now been addressed:

- the **admin plane** and **artifact plane** now fail much more safely by default,
- the **secret update path** and **KEK handling** now use a canonical transport/storage model instead of ambiguous raw bytes and plaintext-at-rest persistence,
- the main **shared-nothing recovery** issues around route replay, bootstrap correlation, gateway-policy snapshot state, and artifact advertisement have been materially corrected,
- the **shutdown lifecycle** now fences instances instead of releasing resources on timeout,
- and the main **WASI filesystem/outbound-network** enforcement gaps identified in the audit have been reduced.

What remains is now mostly a mix of:

- additional regression/integration coverage,
- a few still-open medium-priority hardening items,
- and future-strengthening provenance/trust work beyond the now-implemented authoritative cluster-node registry and signed artifact transfer manifests.

### Priority counts

- [ ] **P0 — Critical:** 4 items
- [ ] **P1 — High:** 8 items
- [ ] **P2 — Medium:** 10 items
- [ ] **P3 — Low:** 1 item

### Most important remediation themes

- [x] Lock down **admin** and **artifact** endpoints before treating the platform as remotely deployable.
- [x] Repair the **secret lifecycle** so CLI -> message bus -> node -> secret provider all agree on a canonical transport and storage format.
- [x] Fix the main **shared-nothing rebuild paths**: route replay, node bootstrap artifact URLs, and durable state recovery primitives.
- [x] Make **instance shutdown authoritative** so routes and ports are not released before Wasm workers are truly gone.
- [x] Tighten the main **runtime enforcement** gaps around filesystem exposure and outbound network policy.

---

## Shared-Nothing Assessment

### What is already aligned

- [x] The platform is clearly designed around **local node state** (`redb`) plus an event/control plane (`NATS/JetStream`) rather than centralized request-path storage.
- [x] Messaging uses versioned envelopes, which is a good fit for rolling upgrades.
- [x] Storage migration and backup behavior are stronger than average for an early platform.
- [x] The runtime is already oriented around Wasmtime Component Model + fuel metering.

### What currently weakens the shared-nothing model

- [x] Route recovery replay now consumes the canonical event envelope format.
- [x] Nodes now advertise routable artifact URLs for real multi-node exchange instead of hard-coded loopback.
- [x] Route-level rate-limit defaults now align with the shared-nothing model: node-local by default, distributed only by explicit opt-in.
- [x] App listeners are bound to loopback by default, with supervisor-enforced bind IP/port checks to keep north-south traffic on the gateway/proxy path unless an operator explicitly changes the runtime bind address.
- [ ] WASI runtime enforcement still leaves optional future hardening work to outer layers and deeper host/resource wrapping; this is not blocking the current non-Windows production baseline.

Overall assessment: the **architecture direction is shared-nothing**, but parts of the **current implementation still behave like a partially trusted prototype** rather than a hardened distributed platform.

---

## Recommended Implementation Order

### Phase 0 — Environment and build hygiene

- [x] Free disk space and rerun `cargo check --workspace`
- [x] Move effective release profile settings to the workspace root
- [x] Capture a clean baseline for `cargo test --workspace --no-run`

### Phase 1 — Security and correctness blockers

- [x] Fix the secret rotation / storage format pipeline
- [x] Stop storing the KEK in plaintext at rest
- [x] Lock down admin API exposure, auth defaults, and TLS enforcement
- [x] Lock down artifact server exposure and access control

### Phase 2 — Shared-nothing recovery and isolation

- [x] Fix route replay to consume the real `MessageEnvelope<Event>` wire format
- [x] Replace localhost artifact URLs with routable node identity / advertised address configuration
- [x] Make instance shutdown authoritative before deregistering routes and ports
- [x] Tighten WASI filesystem and outbound network enforcement
- [x] Revisit direct host exposure of app listeners

### Phase 3 — Operational accuracy and developer trust

- [x] Fix metrics bucketing and JetStream stream-name monitoring
- [x] Align rate-limit defaults with documented shared-nothing behavior
- [x] Repair test harness assumptions (`Podman` vs `Docker`, `tc` scope, WSL helper safety)
- [x] Bring sample apps to realistic Wasm/native parity

---

## Detailed Audit Files

- [x] `INFRA_IMPL/43_PLATFORM_CODE_AUDIT_CORE_FINDINGS.md`
- [x] `INFRA_IMPL/44_PLATFORM_CODE_AUDIT_SUPPORTING_FINDINGS.md`
- [x] `INFRA_IMPL/45_PRODUCTION_READINESS_DEEP_AUDIT.md`
- [x] `INFRA_IMPL/46_PRODUCTION_HARDENING_AND_RELEASE_GATES.md`
- [x] `INFRA_IMPL/47_ARTIFACT_PLANE_IDENTITY_MODEL_ADR.md`

Suggested reading order:

1. Step 43 — immediate platform/security/shared-nothing issues
2. Step 44 — supporting/tooling/test/app issues
3. Step 45 — deeper distributed control-plane and bootstrap correctness audit
4. Step 46 — production hardening and release-gate audit
5. Step 47 — artifact-plane long-term identity follow-up

---

## Strengths Worth Preserving

Do not lose these while fixing the gaps:

- [ ] Preserve versioned message envelopes in `crates/messaging`
- [ ] Preserve migration backup behavior in `crates/storage`
- [ ] Preserve `Zeroize`-based handling in `crates/secrets`
- [ ] Preserve constant-time token comparison in `crates/common` / `crates/proxy`
- [ ] Preserve conservative cross-namespace defaults in `crates/internal_gateway`
- [ ] Preserve explicit runtime limitation documentation in `crates/runtime/POLICY_LIMITATIONS.md`

---

## Final Assessment

The codebase is now clearly beyond a toy prototype: the subsystem split is coherent, the most important audit findings have implementation work behind them, and the project has a much stronger foundation for a Wasm-first shared-nothing platform than it did at the start of this audit.

The original highest-risk gaps were not cosmetic — they affected:

- secret correctness,
- admin and artifact exposure,
- node rebuild reliability,
- runtime enforcement boundaries,
- and safe lifecycle management of running Wasm instances.

Those base-platform gaps have now been materially reduced or closed. The remaining work is mostly:

- finishing broader validation coverage,
- cleaning up medium-priority operability items from Step 44,
- and deciding how far to go on long-term trust/provenance strengthening, especially for the artifact plane in Step 47.




