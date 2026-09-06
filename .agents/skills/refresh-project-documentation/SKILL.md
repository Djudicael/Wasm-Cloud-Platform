---
name: refresh-project-documentation
description: Audit and synchronize all maintained Wasm Cloud Platform documentation with current code, APIs, manifests, scripts, CI, security policy, deployment behavior, and validation evidence. Preserve every accurate detail, add missing information, correct changed behavior, and remove only claims that repository evidence shows are no longer true. Use after implementation or interface changes, or when project documentation may be stale.
---

# Refresh project documentation

Make the documentation describe verifiable repository reality. This is a
repository-wide consistency pass, not permission to rewrite historical evidence
or invent guarantees.

## Preserve information while refreshing it

Refresh documentation in place. Do not treat a refresh as a rewrite,
consolidation, shortening exercise, or style cleanup. Existing architecture,
rationale, API descriptions, examples, troubleshooting steps, limitations, and
security guidance are valuable when they remain accurate.

Classify each affected statement or section before editing it:

- **Retain** accurate information, even when it is detailed, repetitive, old in
  writing style, or more technical than a summary would be.
- **Correct** the smallest relevant passage when an API, command, configuration
  key, behavior, version, or operational procedure changed.
- **Add** newly implemented behavior and information that readers need but the
  current documentation omits. Extend the relevant existing section before
  creating another summary document.
- **Remove** information only when an authoritative repository source proves it
  is no longer implemented, supported, available, or true. Also remove content
  that is demonstrably misplaced duplication, while confirming and reporting
  where the canonical copy remains.

Do not remove material merely because it is verbose, highly technical,
duplicated for a useful audience, difficult to verify quickly, or absent from a
single source file. Follow references and callers before deciding that a feature
or explanation no longer exists. If a section mixes accurate and stale claims,
preserve its structure and edit the individual claims instead of replacing the
whole section with a short summary.

Before accepting a substantial deletion, inspect the focused before/after diff
and account for the removed information. Restore the original and redo the edit
surgically if the deletion removes accurate context, examples, rationale, or
operational detail. In the final report, explain every intentional large
deletion and identify the evidence or canonical destination that justifies it.

When a technical relationship, lifecycle, data flow, trust boundary, or
multi-component procedure is hard to understand in prose, add or update a
schema. Prefer Mermaid for a compact maintained software diagram when the
renderer supports it; use a text diagram or table when that is more portable.
Derive nodes, arrows, states, and labels from the implementation and keep the
surrounding prose for details the diagram cannot express.

## Establish scope and sources of truth

1. Read `AGENTS.md`, inspect `git status`, and preserve unrelated tracked and
   untracked work.
2. Inventory tracked Markdown with `rg --files -g '*.md'`. Treat these groups
   differently:
   - primary platform manual: every maintained file under `docs/`. This is the
     user and operator documentation for installing, configuring, running,
     securing, observing, and using the platform. Audit it as one connected
     manual, not as optional supporting material;
   - maintained entry points and component guides: the documentation index in
     `README.md`, `SECURITY.md`, `apps/README.md`, crate README files, and current
     `INFRA_IMPL/process/` checklists and procedures;
   - implementation/design records: numbered `INFRA_IMPL/*.md` files, which may
     contain both current contracts and historical decisions;
   - historical or run-specific material: `STUDIES/` and
     `INFRA_IMPL/process/prod_validation/evidence/`. Preserve recorded results,
     dates, hashes, logs, and conclusions. Add a clearly dated correction or
     superseding note when necessary instead of rewriting evidence;
   - generated files and local evidence: regenerate with the owning script and
     do not commit them unless the user or repository policy requires it.
3. Resolve conflicts using authoritative sources:
   - manifests, `Cargo.lock`, and `rust-toolchain.toml` for versions and targets;
   - public source, configuration structs, CLI definitions, tests, and examples
     for behavior and interfaces;
   - `.github/workflows/`, `scripts/`, and `.agents/skills/` for executable
     workflows;
   - `.cargo/audit.toml`, `deny.toml`, `SECURITY.md`, and
     `INFRA_IMPL/process/DEPENDENCY_SECURITY_EXCEPTIONS.md` for dependency
     security policy;
   - current validation plans and evidence for what was actually exercised.

## Audit for drift

Search exact changed names and values first, then inspect related prose for:

- old Rust, crate, application, protocol, schema, or release versions;
- commands that disagree with CI, omit WSL requirements, use the wrong working
  directory, or name missing packages, targets, scripts, flags, or files;
- stale configuration keys, environment variables, CLI options, ports,
  endpoints, health semantics, deployment steps, and teardown procedures;
- public API changes, including renamed or added commands, arguments, types,
  methods, endpoints, request/response fields, defaults, errors, and lifecycle
  behavior. Document the new interface and any migration impact wherever users
  or operators encounter the old interface;
- mismatches among architecture diagrams, crate READMEs, operator guides,
  implementation records, and application deployment examples;
- gaps or contradictions in the `docs/` manual's end-to-end journeys: choosing a
  deployment level, installing and configuring the platform, starting required
  services, deploying an application, routing traffic, securing endpoints,
  observing health, upgrading, recovering, and removing resources;
- undocumented security exceptions, production limitations, failure behavior,
  or observability requirements;
- broken relative links and references to moved or deleted files;
- claims that a Firecracker rehearsal proves production readiness. MicroVMs are
  a platform testbed; external HA, durable services, production PKI/secrets,
  provider behavior, and real capacity remain operator validation concerns.

Do not mechanically replace literals across historical documents. Determine
whether each match states a current contract, an example, or a past observation.

## Synchronize maintained documentation

1. Update every maintained document affected by the verified change, including
   root, component, operator, security, release, and agent guidance where
   applicable.
   Preserve unaffected content and the document's useful level of detail.
2. Treat `docs/` as the canonical manual for human platform usage. Keep its
   commands, prerequisites, examples, configuration, operational warnings, and
   navigation complete. Ensure the root `README.md` documentation index exposes
   the current manual entry points, and link new or renamed manual pages from a
   discoverable index or related guide.
3. Prefer one canonical source with links from other documents. Avoid copying
   volatile dependency tables or long procedures into multiple files. Do not
   use this rule to delete audience-specific explanation that remains useful and
   accurate; replace only genuinely misplaced duplication with a clear link to
   its canonical destination.
4. Keep commands copy-pasteable in their documented shell. Rust builds and tests
   must use Linux/WSL2; for this checkout use
   `CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target`.
5. Distinguish implemented, compiled, unit-tested, integration-tested,
   microVM-validated, and production-supported behavior.
6. Keep the node-local service-mesh boundary explicit: same-node `.internal`
   routing is the platform design; cross-host mesh identity is out of scope.
7. When changing an agent workflow, update `AGENTS.md` and the relevant
   repository skill. When changing a canonical script, update human-facing
   documentation that delegates to it.
8. Preserve security exception ownership, reachability analysis, review date,
   and removal condition. Never make a warning disappear only in prose.

## Validate

1. Search again for stale values and inspect every remaining match. Report
   intentionally preserved historical matches.
2. Confirm referenced paths with `Test-Path` or `rg --files`, and validate Cargo
   package names with `cargo metadata --locked --no-deps` in WSL.
3. Walk the manual from its root `README.md` entry points and verify that every
   maintained `docs/` page is discoverable, internal links resolve, sequential
   instructions agree, and safe `--help`, metadata, or dry-run commands match
   the documented interface. Do not execute destructive or production commands
   merely to validate prose.
4. Run configured Markdown/link/documentation checks if present. Do not invent a
   checker or silently install one.
5. Run doctests or compile examples when documentation changes executable Rust
   examples or public APIs.
6. Inspect `git diff --check`, the final documentation diff, and `git status` for
   broken formatting, contradictory claims, accidental evidence edits, and
   unrelated churn. Review `git diff --stat` or `git diff --numstat` and inspect
   every document with substantial deletions to confirm that the refresh did
   not silently simplify or discard accurate information.

Report the maintained files updated, facts synchronized, checks performed,
historical records deliberately preserved, and any claim that could not be
verified.
