# Repository agent guidance

## Environment

- Run Rust builds and tests inside Linux or WSL2. When the checkout is under `/mnt/<drive>`, set `CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target` to keep build artifacts on the Linux filesystem.
- Use the pinned toolchain from `rust-toolchain.toml` and keep `Cargo.lock` committed.
- Preserve unrelated worktree changes. Never delete microVMs, processes, bridges, or state by broad pattern matching.

## Required checks

- After Rust changes, run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the relevant tests in WSL.
- Check native workspace targets with `cargo check --workspace --all-targets --exclude http-hello-component --exclude wasi-grpc-echo`.
- Build WASI applications explicitly for `wasm32-wasip2` because native workspace checks do not exercise their WASI-only dependency paths.
- This repository currently has no JavaScript/TypeScript frontend manifest. Do not invent frontend dependency updates; only run frontend package-manager commands if a real manifest and lockfile are present.

## Local microVM test environment

Repository-scoped Open Agent Skills live in `.agents/skills` and are intended for Codex, Claude Code, OpenCode, and other agents that implement or can read the Agent Skills format.

- Use `$provision-microvm-testbed` to create a local Firecracker topology and persist its exact state.
- Before provisioning, ask for the desired platform-node count when it is not explicit. Clarify that the separate NATS microVM is not part of this count.
- For a close-to-production local rehearsal, use `--preset production-like --nodes N` with at least three nodes. This adds a host HAProxy front door in front of the reverse proxy embedded in every platform node. Do not describe it as production-ready: TLS, external secrets, observability, and highly available NATS remain operator concerns.
- Use `$deploy-test-application` to build, deploy, route, and verify a Wasm application in that topology.
- Use `$destroy-microvm-testbed` after testing to stop only the recorded VMs and remove their network/state.
- The skills delegate operations to the canonical `scripts/vm/provision-testbed.sh`, `scripts/vm/deploy-test-application.sh`, and `scripts/vm/destroy-testbed.sh` entrypoints. Use those scripts directly for human-driven automation.
- Run skill scripts in WSL2. Keep the same `--state-file` value across all three skills.
- Never point these local-testing skills at production infrastructure.
