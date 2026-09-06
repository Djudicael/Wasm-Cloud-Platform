---
name: update-rust-dependencies
description: Audit, update, and validate Rust dependencies and the pinned Rust toolchain across the Wasm Cloud Platform workspace, WASI applications, and eBPF workspace. Use for Cargo upgrades, Rust version bumps, deprecated or vulnerable crate replacement, lockfile refreshes, or dependency-related CI failures.
---

# Update Rust dependencies

Treat dependency updates as compatibility and security work, not a blind
version sweep. Use authoritative current release notes, registries, advisories,
and project documentation; do not rely on remembered versions.

## Establish constraints

1. Read `AGENTS.md` and inspect the worktree. Preserve unrelated tracked and
   untracked changes.
2. Inspect the root `Cargo.toml`, every member manifest, `Cargo.lock`,
   `rust-toolchain.toml`, `.cargo/`, `deny.toml`, CI/release workflows, and the
   independent `crates/ebpf-monitor/bpf/Cargo.toml` and lockfile.
3. Derive the actual validation matrix from `.github/workflows/ci.yml`,
   `.github/workflows/release.yml`, and relevant scripts. Do not assume a native
   workspace check covers WASI or eBPF targets.
4. Work inside Linux/WSL2 and set
   `CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target` for the checkout under
   `/mnt/d`. Keep committed lockfiles and use the pinned toolchain.
5. This repository currently has no JavaScript/TypeScript frontend manifest.
   Do not invent frontend upgrades or run a package manager unless a real
   manifest and lockfile are present when the skill is invoked.

## Plan the update

Classify each candidate before editing:

- lockfile-only update within existing manifest requirements;
- compatible direct dependency update;
- breaking API, feature, MSRV, target, or behavior migration;
- security/deprecation replacement;
- blocked by Rust version, WASI/eBPF support, licensing, upstream pins, or a
  required backend.

Prioritize reachable security fixes and direct dependencies. Group closely
related ecosystem crates, such as Wasmtime/WASI, Pingora, Tokio, AWS, telemetry,
or Sigstore, so their versions and features remain compatible. Ask before a
replacement that materially changes public behavior, cryptographic/TLS
backends, licensing, persistence formats, or supported targets unless the user
already authorized that change.

For a Rust toolchain bump, verify the new compiler supports every required host,
`wasm32-wasip2`, and the nightly/build-std eBPF flow. Update documentation and CI
pins that intentionally mirror the toolchain; do not replace independent pins
without checking their purpose.

## Apply changes

1. Centralize versions in `[workspace.dependencies]` when dependencies are
   genuinely shared. Preserve intentional target-specific dependencies,
   features, and `default-features` settings.
2. Use Cargo to update lockfiles; never hand-edit them. Use precise package
   updates for attributable changes before considering a full lockfile refresh.
3. Update source, tests, examples, and configuration for API migrations.
4. Review `cargo tree` for unexpected feature activation, duplicate major
   versions, crypto/TLS backend changes, native dependencies, and target-specific
   paths. Check both the main and eBPF manifests when affected.
5. Do not silence advisories casually. Keep `.cargo/audit.toml`, `deny.toml`, and
   `INFRA_IMPL/process/DEPENDENCY_SECURITY_EXCEPTIONS.md` synchronized. Every
   retained exception needs a dependency path, reachability assessment, owner,
   review deadline, and removal condition. Remove stale exceptions when the
   affected package leaves the graph.
6. Run `$refresh-project-documentation` when available after versions, MSRV,
   commands, features, security policy, or supported behavior change; otherwise
   perform the equivalent documentation consistency pass.

## Validate in WSL

Run the repository-required checks relevant to the change, including at least:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets \
  --exclude http-hello-component --exclude wasi-grpc-echo
cargo test --workspace --lib --bins
cargo build --manifest-path apps/http-hello-component/Cargo.toml \
  --target wasm32-wasip2
cargo build --manifest-path apps/wasi-grpc-echo/Cargo.toml \
  --target wasm32-wasip2
cargo audit --deny warnings
cargo deny check advisories bans sources
```

Also:

- compile and run focused tests for every migrated crate;
- build other affected WASI applications explicitly for `wasm32-wasip2`;
- use `scripts/ebpf/install-toolchain.sh` and
  `scripts/ebpf/build-ebpf.sh` when Rust or eBPF dependencies change;
- validate the separate eBPF lockfile with its manifest;
- run release supply-chain checks when lockfiles, audit policy, toolchain, or
  release inputs change;
- run service-backed E2E or microVM validation only when the update affects those
  paths and prerequisites are available.

When a check fails, identify whether the cause is the update, existing state,
the environment, unavailable infrastructure, or a flaky test. Fix in-scope
regressions and report the exact remaining failure. Do not weaken a gate merely
to make CI green.

## Report

Summarize direct and important transitive changes, migrations and intentional
feature changes, blocked candidates, policy exceptions, all commands and
outcomes, and remaining risks for Rust compatibility, WASI, eBPF, TLS/crypto,
native libraries, and integration behavior. Do not claim all dependencies are
current unless every manifest and relevant target was checked against current
authoritative sources.
