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

- Use `$refresh-project-documentation` after changes that can make maintained
  documentation stale. Treat `docs/` as the primary platform usage and operator
  manual, keep its root README navigation current, preserve historical
  production-validation evidence, and distinguish local microVM validation from
  production guarantees. Refresh documentation surgically: retain all accurate
  technical detail, correct changed behavior, add missing APIs and operational
  information, and remove only claims disproven by authoritative repository
  sources. Review substantial deletions explicitly. Add or update Mermaid, text
  diagrams, or tables when they make complex flows and trust boundaries easier
  to understand without replacing the supporting explanation.
- Use `$update-rust-dependencies` for Cargo dependency updates, Rust toolchain
  bumps, advisory remediation, and dependency-policy CI failures. It covers the
  main workspace, explicit WASI targets, and the separate eBPF workspace.

- Use `$provision-microvm-testbed` to create a local Firecracker topology and persist its exact state.
- Before provisioning, ask for the desired platform-node count when it is not explicit. Clarify that the separate NATS microVM is not part of this count.
- For a close-to-production local rehearsal, use `--preset production-like --nodes N` with at least three nodes. This adds a host HAProxy front door in front of the reverse proxy embedded in every platform node. Do not describe it as production-ready: TLS, external secrets, observability, and highly available NATS remain operator concerns.
- Use `$deploy-test-application` to build, deploy, route, and verify a Wasm application in that topology.
- Service microVMs are not platform nodes. PostgreSQL uses `scripts/vm/build-postgres-rootfs.sh` and `scripts/vm/provision-postgres-service.sh`, and its lifecycle is recorded in the same state file.
- A real local Vault Transit rehearsal uses `scripts/vm/build-vault-rootfs.sh`,
  `scripts/vm/provision-vault-service.sh`, and
  `scripts/vm/validate-vault-transit-microvm.sh`. Vault runs as a separate
  sealed service microVM with TLS and AppRole-derived least-privilege tokens;
  its test credentials remain in the recorded state-scoped runtime directory.
  Follow `docs/vm-testbed/service-microvms.md` for sensitive-artifact handling,
  idempotent reprovisioning, scope boundaries, and teardown behavior.
- Production-validation rehearsals can add the disposable Podman observability stack with `scripts/vm/provision-observability.sh`; its exact container identities and runtime directory are recorded in the companion service state and removed by the canonical teardown script. Follow `INFRA_IMPL/process/PRODUCTION_TELEMETRY_VALIDATION.md` for the trace/log/audit contract, outage tests, and the explicit Collector-outage durability boundary.
- Validate the tracked Prometheus rules and state-scoped Alertmanager delivery path with `scripts/vm/validate-alerting.sh`. Follow `INFRA_IMPL/process/PRODUCTION_ALERTING_VALIDATION.md`; the local webhook recorder is evidence tooling, not a production receiver.
- For the two-WASI OpenID Connect Hub rehearsal, use `scripts/vm/deploy-oidc-hub-test.sh`; it runs migrations and configures the recorded HAProxy with the required same-origin route split.
- The application internal mesh is node-local by design. Use literal
  `<app>.<namespace>.internal` URLs, `placement.policy = "every_node"`, and
  same-namespace `local_dependencies` to co-locate dependency closures. Never
  add remote-node lookup or forwarding fallback for `.internal`; cross-host
  mesh identity is explicitly out of scope. Remote application traffic must use
  an explicit external endpoint and its separately validated security policy.
- Use `$destroy-microvm-testbed` only after the user is finished testing. If interactive testing was requested, leave the environment running until teardown is explicitly requested.
- The skills delegate operations to the canonical scripts under `scripts/vm/`. Use those scripts directly for human-driven automation.
- Run skill scripts in WSL2. Keep the same `--state-file` value across all three skills.
- Never point these local-testing skills at production infrastructure.
- For production planning, use `INFRA_IMPL/process/PLATFORM_PRODUCTION_DEPLOYMENT_CHECKLIST.md`; it is an operator gate, not authorization for agents to mutate production systems.
