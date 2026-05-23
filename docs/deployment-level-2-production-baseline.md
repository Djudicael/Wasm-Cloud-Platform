# Level 2 - Production Baseline

Use this guide for the first real Linux production rollout. This is the current production baseline for the project.

See also: [`deployment-levels.md`](deployment-levels.md)

## When To Choose This Level

- the platform serves real production traffic
- you need a credible Linux production baseline
- you want the audited baseline without every optional hardening feature

## What To Install

1. the platform binaries
2. NATS with JetStream and authentication enabled
3. your chosen persistent secret/root-of-trust backend
4. Linux service management and log collection for the node

## Starting Config

Start from:

- `config/production.toml`

Recommended choices:

- `auth.enabled = true`
- `auth.require_tls = true`
- dedicated admin TLS, or shared proxy TLS fallback
- admin API on loopback unless intentionally fronted by a trusted proxy
- `auth.trusted_proxies` only when you actually have one
- artifact server loopback bind with authenticated peer transfer
- loopback instance bind address
- NATS authentication enabled
- persistent seal-key source, in this order:
  - `vault-transit`
  - `aws-kms-hmac`
  - `vault-kv`
  - `command`
  - `passphrase-env:...`

## Setup Steps

1. copy `config/production.toml` to an environment-specific config
2. set real storage paths, NATS endpoints, and node identity
3. configure admin TLS or the shared proxy TLS fallback
4. choose a production seal-key source
5. enable authenticated NATS credentials
6. keep app instances loopback-bound
7. keep the artifact plane authenticated
8. start the node and verify health before deploying apps

## Settings To Choose Carefully

- do not expose the admin path without TLS
- do not configure `auth.trusted_proxies` unless the immediate upstream is actually trusted
- do not use `generate` for the seal key
- do not expose the artifact plane publicly without auth

## Validation

- run real CI before release
- run a production-like Linux rehearsal
- verify restart continuity for secrets and node state
- verify deploy, undeploy, and rolling upgrade flows
- verify metrics scraping and logging in the real environment

## Move To Level 3 When

- you are operating multiple nodes
- the admin plane is fronted by infrastructure you control
- you need stronger operational trust, CI coverage, and alerting

Next guide:

- [`Level 3 - Hardened Production`](deployment-level-3-hardened-production.md)
