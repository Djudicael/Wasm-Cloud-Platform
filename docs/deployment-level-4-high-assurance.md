# Level 4 - High-Assurance

Use this guide when you want the strongest Linux posture the current codebase supports without adding new platform features.

See also: [`deployment-levels.md`](deployment-levels.md)

## When To Choose This Level

- you already meet the Level 3 bar
- you want the strongest currently supported posture
- you are willing to accept higher operational discipline

## What To Install

1. everything from Level 3
2. dedicated admin TLS material
3. formal operator runbooks and release rehearsal process

## Starting Config

Start from:

- the Level 3 production config you already validated

Recommended additions:

- standardize on `vault-transit` or `aws-kms-hmac`
- use `command` only when an external broker forces it
- dedicated admin TLS material instead of shared proxy fallback
- strict trusted-proxy configuration with no unnecessary forwarded-header trust
- production cache directory enabled
- pooling allocator left disabled unless your own benchmark proves it helps

## Setup Steps

1. switch to dedicated admin TLS material if you still rely on proxy fallback
2. standardize the seal-key source on `vault-transit` or `aws-kms-hmac`
3. review every trusted-proxy entry and remove anything unnecessary
4. formalize release rehearsal before each rollout
5. maintain operator runbooks for bootstrap, rollback, secret rotation, node replacement, and incident triage

## Validation

- verify the formal release rehearsal before rollout
- verify rollback from the current release artifact set
- verify operator runbooks are current and usable
- rerun your own Wasmtime cache and allocator review when workload shape changes materially

## What Remains Deferred Even Here

These are future hardening items, not required for the current Linux production target:

- native TPM/HSM SDK integration
- transparency logs / external attestations
- deeper Wasmtime host/resource wrapping for remaining byte-accurate paths
- artifact-plane long-term identity end-state
