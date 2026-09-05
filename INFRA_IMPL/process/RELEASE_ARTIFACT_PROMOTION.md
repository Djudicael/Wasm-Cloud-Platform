# Release artifact promotion and admission

This procedure is the supply-chain gate for platform binaries. A release is
promotable only when it was produced by `.github/workflows/release.yml` from an
approved semantic-version tag and all checks below pass. A manual workflow run
is always labelled `candidate` and must never be promoted.

## Published artifact set

The allowlist is deliberately closed:

- `wasm-node`, built with the `ebpf` feature;
- `wasm-ctl` and `wasm-deploy-ingress`;
- `hello-axum.wasm`, used as the first-party WASI smoke component;
- the seven eBPF objects under `ebpf/`;
- `sbom.spdx.json`, blocking `security-audit.json`, `SHA256SUMS`, and
  `RELEASE-MANIFEST.json`.

Any missing or additional file causes admission to fail. The manifest binds the
artifact hashes and sizes to the full source commit, release ref, committed
`Cargo.lock`, `rust-toolchain.toml`, tool versions, and `SOURCE_DATE_EPOCH`.

## CI build and attestation gate

The release workflow:

1. checks out the exact `github.sha`, requires a clean tree, and confirms the
   checkout matches GitHub's immutable source identity;
2. accepts production promotion only for a `vMAJOR.MINOR.PATCH` tag; manual runs
   produce non-promotable candidates;
3. uses the pinned Rust 1.97.1 toolchain, pinned eBPF nightly and linker, both
   committed lockfiles, and `--locked --frozen` builds;
4. creates a deterministic gzip/tar archive using the commit timestamp;
5. records the pinned `cargo-audit` result and the exact deduplicated exception
   set from `.cargo/audit.toml`, binds that policy file's SHA-256 into the
   manifest, and verifies that the manifest matches the audit evidence;
6. generates an SPDX 2.3 SBOM with pinned Anchore Syft automation;
7. creates GitHub OIDC/Sigstore SLSA provenance and SPDX attestations for every
   subject, plus provenance for the promotion archive;

Fresh hosted runners intentionally fetch the dependency archives named by the
committed workspace lockfile with `cargo fetch --locked`. The eBPF fetch uses
the pinned nightly plus `-Z build-std=core` so it also resolves the pinned Rust
sysroot crates required for the BPF target. Resolution is then checked offline
and every release build remains `--locked --frozen`.
Starting with `cargo metadata --frozen` on an empty runner is invalid because
`--frozen` implies offline mode; it can report that even foundational crates are
missing before it has had an opportunity to populate the cache.

The seven eBPF outputs are non-empty ELF data objects, not host executables.
Their build gate checks regular-file presence and content; release staging
normalizes them to mode `0644`. Do not use an executable-bit test, because WSL
DrvFs and native Linux filesystems report that bit differently.

An audit passing under the repository policy means that no *unexcepted*
advisory or warning was found. It must not be described as an exception-free
dependency graph. Each retained exception needs a documented dependency path,
reachability assessment, owner, review date, and removal condition. The RSA
backend advisory `RUSTSEC-2023-0071` is not accepted: JWT verification uses the
`aws-lc-rs` backend so that vulnerable `rsa` crate is absent from the release
graph. Current Pingora constraints still require the scoped protobuf and `lru`
exceptions recorded by `.cargo/audit.toml`; their presence is visible in both
`security-audit.json` and `RELEASE-MANIFEST.json`. The complete exception
rationale, review deadline, ownership, and removal conditions are maintained in
[`DEPENDENCY_SECURITY_EXCEPTIONS.md`](DEPENDENCY_SECURITY_EXCEPTIONS.md).

8. verifies source commit, source ref, signer workflow, provenance predicate,
   and SPDX predicate before uploading the workflow artifact.

GitHub artifact attestations require a supported GitHub plan. Public repositories
are supported; private/internal repositories require the applicable GitHub
Enterprise Cloud capability. If that service is unavailable, this gate fails
closed: do not bypass it by publishing an unattested archive.

## Operator admission

Download the archive, extracted `release-artifacts/` directory, and attestation
evidence from the same successful workflow run. In WSL/Linux, run:

```bash
bash scripts/verify-release-attestations.sh \
  wasm-cloud-platform-release.tar.gz \
  release-artifacts \
  OWNER/REPOSITORY \
  FULL_40_CHARACTER_GIT_SHA \
  refs/tags/vMAJOR.MINOR.PATCH
```

This requires an authenticated GitHub CLI. It first rejects unsafe archives,
unexpected files, symlinks, checksum/size changes, an invalid manifest, or a
missing/non-SPDX-2.3 SBOM. It then verifies both SLSA provenance and SPDX
attestations against the expected repository, source SHA, source tag, and exact
release workflow identity.

Deploy by recorded digest, never by an unqualified filename, mutable branch, or
moving tag. Preserve the workflow URL, run ID, tag, source SHA, archive digest,
manifest, SBOM, and attestation bundles with the production change record.

## Evidence boundary

Local script tests prove deterministic packaging and fail-closed tamper handling.
They cannot mint or validate a GitHub OIDC identity. P10-01 is fully closed for a
specific release only after a clean tag workflow succeeds and an independent
operator repeats the admission command on the downloaded bytes.
