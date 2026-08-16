---
name: provision-microvm-testbed
description: Provision a local Wasm Cloud Platform test environment with Firecracker microVMs under Linux or WSL2. Use when an agent needs a single-node, multi-node, chaos-ready, or production-like topology, optionally with an HAProxy front door, before local deployment or testing. Do not use for production infrastructure.
---

# Provision the microVM testbed

1. Before provisioning, determine the exact positive integer number of platform nodes the user wants. If it was not supplied, ask; do not silently choose a node count on the user's behalf. Also ask which topology they want (`smoke`, `multi-node`, or `production-like`) and whether traffic should enter directly through node proxies or through HAProxy, including the desired host bind address. If they say "full" or "production-ready," clarify whether they also expect TLS, an external secrets backend, observability, or highly available NATS; the current local automation does not provision those controls.
2. Explain the topology before running it. Every platform node contains the built-in reverse proxy. The optional `haproxy` front door runs on the WSL/Linux host and balances requests across those node proxies. One additional NATS microVM is always created and is not included in `--nodes`.
3. Work from the repository root and run all build and test commands inside Linux or WSL2.
4. Check that `/dev/kvm` exists and that the user can use `sudo`. Do not weaken KVM permissions automatically.
5. Preserve an existing state file. If `.vm-testbed-state.json` already exists, inspect it with the testbed `status` command and ask before replacing a running environment.
6. Run `bash scripts/vm/provision-testbed.sh --preset PRESET --nodes COUNT`. Pass `--prepare-assets` only when Firecracker, the kernel, or rootfs images are missing and the user accepts the longer setup. This script reuses the existing installers and image builders in `scripts/vm/`.
7. Use `--preset smoke --nodes 1` for a fast check, `--preset multi-node --nodes N` for routing and replication, or `--preset production-like --nodes N` for at least three nodes, higher per-node memory, chaos controls, and an HAProxy front door. Use `--front-door none` if the user declines the external load balancer, or `--front-door haproxy --front-door-bind HOST:PORT` to select it explicitly.
8. Report the state-file path, node count, NATS endpoint, node admin/proxy addresses, and front-door address printed by the command. Keep both the VM state file and its `.services.json` companion for the deploy and destroy skills.

The production-like preset is a local rehearsal, not a production deployment. It does not create production TLS certificates, a persistent external secrets backend, monitoring/alerting, or a highly available NATS cluster. State these gaps rather than claiming production readiness.

Treat provisioning as successful only when every node passes the CLI health wait and `status` reports live processes with successful health responses. On failure, preserve logs and state for diagnosis; do not claim the environment is ready.
