# Deployment Levels

This document is the entry point for deployment guidance. Each deployment level now has its own file so operators can follow a path that matches their environment instead of reading one mixed guide.

The project targets **Linux**. Windows is not a production target for this platform because the runtime, observability, and isolation model depend on Linux kernel capabilities, including eBPF.

## Choose Your Starting Point

- developer laptop: [`Level 0 - Local Development`](deployment-level-0-local-development.md)
- internal single-node service: [`Level 1 - Single-Node Private Environment`](deployment-level-1-single-node-private.md)
- minimum production control profile: [`Level 2 - Production Baseline`](deployment-level-2-production-baseline.md)
- hardened multi-node control profile: [`Level 3 - Hardened Production`](deployment-level-3-hardened-production.md)
- high-assurance control profile: [`Level 4 - High-Assurance`](deployment-level-4-high-assurance.md)

## How To Use These Guides

1. choose the lowest level that honestly matches your environment
2. use that file as the install and configuration path for the current rollout
3. only move to the next level when the added controls are worth the operational cost

## Production claim boundary

For this codebase as it stands today:

- **Level 2** is the minimum production control profile
- **Level 3** adds hardened multi-node controls
- **Level 4** adds the strictest controls documented by this repository

Selecting a level does not make a deployment production-ready. Apply the
[production deployment checklist](../INFRA_IMPL/process/PLATFORM_PRODUCTION_DEPLOYMENT_CHECKLIST.md)
to the exact signed release, configuration, hosts, NATS cluster, PKI, secret
backend, load balancer, observability stack, capacity plan, and recovery plan.
The local Firecracker testbed supplies rehearsal evidence only.

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

## Repository and operator boundaries

The release workflow now produces and verifies SLSA provenance and SPDX
attestations. A GA release still requires an approved semantic-version tag and
independent admission of the downloaded bytes. Native TPM/HSM SDK integration,
stronger artifact-plane identity, and process-per-application isolation remain
outside the implemented platform boundary. Operators must retain any resulting
deployment gate rather than treating a profile choice as evidence.
