# Lunaris release runbook

How to cut a Lunaris release. The examples below use v0.7.0 going out
on top of the shipped v0.6.2 (the current workspace version); future
patch / minor releases follow the same flow with the version bumped.
Deployment-side upgrade/rollback (as opposed to registry artifacts) is
§7.

Audience: the release captain. Most steps are verifiable by checking
the artifact's output; the human gate is the "GO / NO-GO" review at
step 6.

## TL;DR for v0.7.0

```bash
# 0. Branch is green
git checkout v0.7.0 && git pull
make ci-local                                # fmt + clippy + test + verify-small/large

# 1. Workspace version is the one CHANGELOG documents
grep '^version' Cargo.toml | head -1         # must be 0.7.0

# 2. Tag + push
git tag -a v0.7.0 -m "v0.7.0 — Moon-only storage"
git push origin v0.7.0

# 3. CI runs and stays green on the tag (semver-checks vs v0.6.2,
#    cargo publish --dry-run on lunaris-core)

# 4. crates.io publish runs in CI: .github/workflows/crates-publish.yml
#    triggers on the v* tag (workflow_dispatch for an initial/repair run —
#    a tag run executes the workflow file AT the tag's commit). It
#    publishes moondb from vendor/moon/sdk/rust first, then the 15
#    publishable crates in dev-dep-aware topological order (derivation:
#    scripts/topo_order.py — versioned dev-deps are KEPT in published
#    manifests, so they order the publish too). Idempotent: re-runs skip
#    versions already on crates.io. Requires the CARGO_REGISTRY_TOKEN
#    repo secret with publish rights to lunaris-* AND moondb.
#
#    GUARD: the workspace `moon = { ..., version = "X" }` pin must equal
#    the version vendor/moon/sdk/rust declares — the job fails fast
#    otherwise. If the vendored moondb API drifted since the last moondb
#    release, cut a new moondb version in the moon repo FIRST (bump
#    sdk/rust/Cargo.toml on moon main, push, bump the submodule + pin
#    here). v0.3.0 was blocked exactly this way: vendored moondb had the
#    3-way hybrid_search, crates.io moondb 0.1.1 only the 2-way.
#    v0.4.0 hit the same pattern: vendored moondb 0.2.1 added
#    HybridFilter + filter param on hybrid_search; the pin was bumped
#    from 0.2.0 → 0.2.1 before tagging. Standing risk noted in the GA
#    plan: crates.io moondb releases are cut by the Moon repo owner —
#    if the vendored SDK drifted, that release happens FIRST.

# 5. Publish Python wheels
cd ../lunaris-py
maturin publish --skip-existing

# 6. Publish TypeScript .node binaries
cd ../lunaris-ts
npm publish --access public

# 7. Run smoke tests against the published artifacts (needs a live
#    Moon — storage is Moon-only since 0.7.0)
make bench-public   # optional
```

## 1. Pre-flight checklist

Before tagging:

- [ ] `make ci-local` is green on the release branch.
- [ ] `CHANGELOG.md` has a current entry for this version with the
      breaking-change section filled in (if any). No "Unreleased" header
      remains.
- [ ] `docs/migration/0.X-to-0.Y.md` exists if this is a minor bump.
- [ ] Every workspace crate's `version.workspace = true` resolves to
      the target version: `cargo metadata --no-deps --format-version 1
      | jq '.packages[] | select(.name | startswith("lunaris")) | .version'`
- [ ] No `0.X.Y-dev` versions anywhere.
- [ ] The README "Status" table reflects this release.
- [ ] LICENSE file present at the repo root.
- [ ] The `v0.4.0` branch is rebased on `main` (or `main` is fast-
      forward-able to it) — release tags live on `main` or a
      release branch, never on a feature branch.

## 2. SemVer discipline

`cargo-semver-checks` runs in CI against the previous tag's baseline
for `lunaris-core` + `lunaris`. If the check fails:

- **Major bump (0.X → 0.Y)**: intentional breaking change. Document it
  in CHANGELOG.md "Breaking" and migration guide; the check fail is
  expected; override by bumping the workspace version to the new minor
  before re-running the gate.
- **Patch bump (0.X.Y → 0.X.Z)**: SemVer break is a bug. Revert the
  break or re-shape the PR; the patch must be additive.

In 0.x land any minor bump is "breaking allowed" by SemVer. Document
the break in CHANGELOG anyway.

## 3. Publishable surface

The workspace's `publish = false` default means crates opt IN explicitly
by setting `publish = true` (or removing the `publish.workspace = true`
inherit, since the workspace says false).

The publishable set (v0.4.0 snapshot — **re-derive per release** with
`grep -l 'publish' crates/*/Cargo.toml` + `scripts/topo_order.py`; the
0.7.0 Moon-only cut deleted `lunaris-storage-postgres`,
`lunaris-storage-embedded`, and `lunaris-migrate`, and added internal
crates this table predates):

| Crate | Status | Notes |
|---|---|---|
| `lunaris-core` | **publish=true** | Core types — must publish first (every other crate depends on it). |
| `lunaris-storage-postgres` | *deleted in 0.7.0* | Was the OSS-default backend through 0.6.x; Moon-only since 0.7.0 (`docs/migration/0.6-to-0.7.md`). |
| `lunaris-llamacpp` | **publish=true** (cargo default — no `publish` key) | THE inference runtime (llama.cpp-only cutover): granite-r2 Q4_K_M embedder + bge-reranker-v2-m3 Q5_K_M reranker behind the `llamacpp` feature. |
| `lunaris-embed-remote` | **publish=true** (cargo default — no `publish` key) | Ollama HTTP escape-hatch embedder; optional dep behind the umbrella `embed-remote` feature. |
| `lunaris-rerank` | **publish=true** | Reranker trait + `NoopReranker` seam (the cross-encoder impl lives in `lunaris-llamacpp`). |
| `lunaris-extract` | **publish=true** | Extractor (ollama, cloud-api — remote-only since the llama.cpp cutover). |
| `lunaris-verify` | **publish=true** | Verifier (incl. RFC 0006 270M scaffold). |
| `lunaris-consolidate` | **publish=true** | ACT-R consolidator. |
| `lunaris-ingest` | **publish=true** | Ingest pipeline. |
| `lunaris-storage-moon` | **publish=true** | Depends on `moondb` (vendored submodule). The crates.io `moondb` must carry the API the vendored copy exposes — see the §TL;DR step-4 guard. |
| `lunaris-retrieve` | **publish=true** | Calls `moondb` `hybrid_search` directly; same moondb-parity requirement as storage-moon. |
| `lunaris` | **publish=true** | Umbrella crate (`lunaris-memory` on crates.io) — publishes LAST. |
| `lunaris-bench` | internal | Benches; not published. |
| `lunaris-conformance` | internal | Cross-backend test harness. |
| `lunaris-codegen` | internal | Build-time codegen. |
| `lunaris-recipes` | internal | Pre-1.0 recipe surface still churning. |
| `lunaris-server` | internal | HTTP server binary (Helm chart in v0.3). |
| `lunaris-py` | PyPI | Built+published via `maturin publish`. |
| `lunaris-ts` | npm | Built+published via `napi prepublish && npm publish`. |
| `xtask` | internal | Build automation. |

Order matters — every crate must publish AFTER its dependencies are
indexed by crates.io. Two non-obvious rules, both encoded in
`.github/workflows/crates-publish.yml`:

- **Versioned dev-deps count.** Workspace-inherited dev-dependencies
  carry `^X.Y.Z` and are kept in the published manifest; crates.io
  rejects a publish whose dev-dep version doesn't exist yet (the
  0.6.x-era example: `lunaris-ingest` dev-depended on
  `lunaris-storage-embedded`, which therefore had to publish first —
  that crate is gone in 0.7.0, but the ordering rule stands). Path-only
  dev-deps (`lunaris-bench`,
  `lunaris-conformance`) are stripped by cargo and don't order anything.
- **`cargo publish` waits for the index** (≥1.66), so sequential
  publishes in `scripts/topo_order.py`'s order are race-free.

## 4. Multi-platform wheels + .node binaries

Python wheels target the matrix from `.github/workflows/python-prebuild.yml`:
cp311 / cp312 across linux-x86_64, linux-aarch64, macos-universal2,
windows-x86_64. TypeScript `.node` binaries target the matching set
via `napi prepublish`.

Both flows trigger automatically on the tag push (`v0.4.*`). The
`maturin publish` and `npm publish` commands above are the local-fallback
recipe if CI doesn't run.

## 5. Post-release verification

After all artifacts are public:

- `cargo add lunaris-memory@0.7.0` into a fresh `cargo new` project, then `cargo build --locked` — verifies the published umbrella crate. (The bare `lunaris` name on crates.io is an unrelated project; and `examples/quickstart-rs` uses a workspace **path** dep, so building it in-tree does **not** exercise the published crate.)
- `pip install lunaris==0.7.0 && python examples/quickstart-py/quickstart.py`
- `npm install @pilotspace/lunaris@0.7.0 && cd examples/quickstart-ts && npm start`

Each must reach "ingested episode at lsn=..." on a clean machine.

## 6. Registry rollback procedure

Crates.io publishes are irreversible (`cargo yank` only flags the
version; it stays installed for existing lockfiles). PyPI and npm
permit `yank` and `deprecate` respectively.

If a breaking bug ships (0.7.0 as the example):

1. `cargo yank --version 0.7.0 <crate>` for every published crate in the set.
2. `pip-yank` equivalent: open a PyPI ticket (no CLI for yank by default;
   use the project page Web UI).
3. `npm deprecate @pilotspace/lunaris@0.7.0 "see issue #XYZ"`.
4. Ship `0.7.1` with the fix and a follow-up CHANGELOG entry referencing
   the yank.

The bad release's git tag stays — the audit trail is more important
than the published artifact disappearing. This section covers registry
artifacts only; rolling a **deployment** back is §7.

## 7. Deployment upgrade / rollback — 0.6.2 ↔ 0.7.0

> **Rehearsed on: PENDING** — the live rehearsal of both directions is a
> GA-4 gate. Until that field carries a date and an operator name, treat
> this section as a *procedure draft* verified against the code and
> migration docs, not against a production run.

Modeled on the 0.3→0.4 rollback precedent
(`docs/migration/0.3-to-0.4-native-default.md` §4). The full upgrade
story (what migrates, what is lossy) is
[`docs/migration/0.6-to-0.7.md`](migration/0.6-to-0.7.md); this section
is the operational order of operations.

### Upgrade 0.6.2 → 0.7.0

1. **Move data first, while still on 0.6.2.** If anything lives in
   Postgres or SQLite, run `lunaris-migrate` **from the v0.6.2 release
   binary** into your Moon (`--shards 1`) *before* upgrading —
   `lunaris-migrate` was deleted in 0.7.0 and cannot be built from the
   0.7.0 tree (`docs/migration/0.6-to-0.7.md` §6; caveat there: the
   binary attached to the v0.6.2 GitHub release must be the
   0.6.2-built one).
2. Verify the moved data per `docs/migration/0.6-to-0.7.md` §2
   "Verification" *before* touching the deployment.
3. Deploy the 0.7.0 binary with `--storage moon://host:port`. There is
   no schema migration and no role bootstrap; the retired
   `lunaris-server migrate` / `bootstrap-db` subcommands answer with
   the exit ramp rather than a parse error.
4. Keep the 0.6.2 source store **read-only but intact** until the
   rollback window below closes — `lunaris-migrate` never mutates the
   source, so it doubles as your rollback dataset.

### Rollback 0.7.0 → 0.6.2

The binary swap is trivial; the data direction is the trap.

1. **Binary:** redeploy the 0.6.2 artifact (or pin 0.6.2 in
   Cargo.toml / requirements.txt / package.json for SDK embedders).
   0.6.2 accepts `moon://` URLs, so a Moon-backed deployment can roll
   the binary back **in place, same store, no data motion** — this is
   the expected path.
2. **Data written while on 0.7.0 stays in Moon.** There is no
   Moon→Postgres/SQLite reverse migration tool — `lunaris-migrate`'s
   `--from` side reads Postgres/SQLite only. If you rolled back
   specifically to return to a Postgres/SQLite store, the 0.6.2
   deployment resumes from that store's **pre-upgrade state** and the
   0.7.0-era writes are not carried back: reconcile by re-ingesting
   from your sources of record, or accept the gap consciously.
3. **Vector indices / embeddings are compatible in both directions**
   within one embedder configuration (the 0.6→0.7 change is storage
   surface, not the embedding model) — no re-embed is needed for the
   in-place Moon path.
4. Re-run your eval gate (recall smoke + the SLO dashboards in
   `docs/operations/slo.md`) after either direction before declaring
   the move done.

The window in which rollback is cheap is exactly the window in which
the retired 0.6.2 store still exists. Decide its retention up front —
that retention *is* your rollback budget.

## Open questions

These are deliberately not resolved in this runbook; the release
captain decides per release:

- Should `lunaris-server` be in the publishable set? Today it's an
  internal binary; v0.3 ships it as a Docker image + Helm chart so the
  answer is "no for crates.io, yes for ghcr.io".
- Should `lunaris-recipes` publish? Recipe surface is still churning;
  hold until v0.3.
- Should we mirror crates.io publishes to a private registry for the
  v0.2.x "self-hosted" milestone? Not required for OSS; gate when v0.3
  needs it.
