# Deployment Levels

This document is the entry point for deployment guidance. Each deployment level now has its own file so operators can follow a path that matches their environment instead of reading one mixed guide.

The project targets **Linux**. Windows is not a production target for this platform because the runtime, observability, and isolation model depend on Linux kernel capabilities, including eBPF.

## Choose Your Starting Point

- developer laptop: [`Level 0 - Local Development`](deployment-level-0-local-development.md)
- internal single-node service: [`Level 1 - Single-Node Private Environment`](deployment-level-1-single-node-private.md)
- first real Linux production rollout: [`Level 2 - Production Baseline`](deployment-level-2-production-baseline.md)
- serious multi-node production: [`Level 3 - Hardened Production`](deployment-level-3-hardened-production.md)
- strongest currently supported posture: [`Level 4 - High-Assurance`](deployment-level-4-high-assurance.md)

## How To Use These Guides

1. choose the lowest level that honestly matches your environment
2. use that file as the install and configuration path for the current rollout
3. only move to the next level when the added controls are worth the operational cost

## Current Production Claim

For this codebase as it stands today:

- **Level 2** is the production baseline
- **Level 3** is the hardened multi-node production path
- **Level 4** is the strongest currently supported Linux posture

## Seal-Key Source Order For Linux Production

Unless your environment forces a different choice, use this order:

1. `vault-transit`
2. `aws-kms-hmac`
3. `vault-kv`
4. `command`
5. `passphrase-env:VAR_NAME`
6. `file`

Notes:

- `generate` is for local development only
- `command` is the escape hatch for custom brokers or external hardware-backed tooling
- `file` is acceptable when file distribution is already part of your security model, but it is weaker operationally than remote derivation

## Intentionally Deferred

These are future hardening items, not blockers for the current Linux production baseline:

- native TPM/HSM SDK integration
- transparency logs / external attestations
- deeper Wasmtime host/resource wrapping for the remaining byte-accurate paths
- artifact-plane long-term identity end-state
