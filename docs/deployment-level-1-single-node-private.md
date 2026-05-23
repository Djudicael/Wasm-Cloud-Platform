# Level 1 - Single-Node Private Environment

Use this guide when you run one Linux node for a real internal service, but you do not need the full multi-node production posture yet.

See also: [`deployment-levels.md`](deployment-levels.md)

## When To Choose This Level

- you run a single Linux node
- traffic is internal or fronted by a trusted edge
- you want persistent local state
- you want stronger controls than local development, but not full production hardening

## What To Install

1. everything from [`getting-started.md`](getting-started.md)
2. a persistent location for `redb`
3. a persistent seal-key source

## Starting Config

Start from:

- `config/staging.toml`

Recommended choices:

- `auth.enabled = true`
- admin API bound to loopback unless intentionally proxied
- artifact server bound to loopback
- persistent Wasmtime cache directory enabled
- persistent seal-key source:
  - `passphrase-env:VAR_NAME`
  - `file`
  - `vault-kv`
  - `vault-transit`
  - `aws-kms-hmac`
  - `command`

## Setup Steps

1. copy `config/staging.toml` to your environment-specific config
2. set the real `node_id`, storage path, and NATS URL
3. enable admin authentication
4. choose a persistent `runtime.key_source`
5. keep admin and artifact binds on loopback
6. start the node with the updated config

## Settings To Choose Carefully

- do not leave `runtime.key_source = "generate"`
- do not expose the admin API publicly without a deliberate proxy and TLS plan
- do not expose the artifact plane publicly without auth

## Validation

- `wasm-node --validate-config /path/to/config.toml`
- restart the node and confirm secrets survive restart
- deploy and undeploy an app successfully
- confirm admin auth is required

## Move To Level 2 When

- the platform serves real production traffic
- you need a defensible Linux production baseline
- you need TLS on the admin path
- you need authenticated NATS in the real environment

Next guide:

- [`Level 2 - Production Baseline`](deployment-level-2-production-baseline.md)
