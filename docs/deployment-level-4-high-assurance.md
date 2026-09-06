# Level 4 - High-Assurance

Use this guide for the strictest Linux control profile documented by this
repository. It still depends on operator-supplied infrastructure and validation.

See also: [`deployment-levels.md`](deployment-levels.md)

## When To Choose This Level

- you already meet the Level 3 bar
- you need the strictest documented control profile
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

These capabilities remain outside the implemented platform boundary and may be
required by your threat model:

- native TPM/HSM SDK integration
- an external transparency-log policy beyond the workflow's verified GitHub attestations
- deeper Wasmtime host/resource wrapping for remaining byte-accurate paths
- artifact-plane long-term identity end-state
