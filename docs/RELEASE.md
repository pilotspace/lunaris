# Lunaris release runbook

How to cut a Lunaris release. Steps assume v0.2.1 going out; future
patch / minor releases follow the same flow with the version bumped.

Audience: release captain on the `v0.2.x` line. Most steps are
verifiable by checking the artifact's output; the human gate is the
"GO / NO-GO" review at step 6.

## TL;DR for v0.2.1

```bash
# 0. Branch is green
git checkout v0.2.1 && git pull
make ci-local                                # fmt + clippy + test + verify-small/large

# 1. Workspace version is the one CHANGELOG documents
grep '^version' Cargo.toml | head -1         # must be 0.2.1

# 2. Tag + push
git tag -a v0.2.1 -m "v0.2.1 — RC-2 scope alphabet tighten + v0.2 close-out"
git push origin v0.2.1

# 3. CI runs and stays green on the tag (semver-checks vs v0.2.0,
#    cargo publish --dry-run on lunaris-core)

# 4. Publish the 8 unblocked crates to crates.io in topological order.
#    The 3 moondb-blocked crates (storage-moon, retrieve, lunaris
#    umbrella) wait until moondb itself publishes — see §3.
cd crates/lunaris-core         && cargo publish --token "$CARGO_REGISTRY_TOKEN"
cd ../lunaris-storage-postgres && cargo publish --token "$CARGO_REGISTRY_TOKEN"
cd ../lunaris-embed            && cargo publish --token "$CARGO_REGISTRY_TOKEN"
cd ../lunaris-rerank           && cargo publish --token "$CARGO_REGISTRY_TOKEN"
cd ../lunaris-extract          && cargo publish --token "$CARGO_REGISTRY_TOKEN"
cd ../lunaris-verify           && cargo publish --token "$CARGO_REGISTRY_TOKEN"
cd ../lunaris-consolidate      && cargo publish --token "$CARGO_REGISTRY_TOKEN"
cd ../lunaris-ingest           && cargo publish --token "$CARGO_REGISTRY_TOKEN"
# BLOCKED until moondb is on crates.io:
# cd ../lunaris-storage-moon   && cargo publish --token "$CARGO_REGISTRY_TOKEN"
# cd ../lunaris-retrieve       && cargo publish --token "$CARGO_REGISTRY_TOKEN"
# cd ../lunaris                && cargo publish --token "$CARGO_REGISTRY_TOKEN"

# 5. Publish Python wheels
cd ../lunaris-py
maturin publish --skip-existing

# 6. Publish TypeScript .node binaries
cd ../lunaris-ts
npm publish --access public

# 7. Run smoke tests against the published artifacts
make bench-public PG_URL=postgres://localhost/lunaris   # optional
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
- [ ] The `v0.2.1` branch is rebased on `main` (or `main` is fast-
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

The v0.2.x publishable set:

| Crate | Status | Notes |
|---|---|---|
| `lunaris-core` | **publish=true** | Core types — must publish first (every other crate depends on it). |
| `lunaris-storage-postgres` | **publish=true** | OSS-default backend. `lunaris-storage-moon` is an OPTIONAL dep behind the `moon-it` feature. |
| `lunaris-embed` | **publish=true** | Embedders (candle, ollama). |
| `lunaris-rerank` | **publish=true** | Cross-encoder reranker. |
| `lunaris-extract` | **publish=true** | Extractor (candle, ollama, cloud-api). |
| `lunaris-verify` | **publish=true** | Verifier (incl. RFC 0006 270M scaffold). |
| `lunaris-consolidate` | **publish=true** | ACT-R consolidator. |
| `lunaris-ingest` | **publish=true** | Ingest pipeline. |
| `lunaris-storage-moon` | BLOCKED | Depends on `moondb` (path-only, sibling repo). Publish `moondb` to crates.io FIRST, then flip. |
| `lunaris-retrieve` | BLOCKED | Transitively depends on `lunaris-storage-moon`. Unblocks once that one publishes. |
| `lunaris` | BLOCKED | Umbrella crate — transitively depends on `lunaris-storage-moon`. |
| `lunaris-bench` | internal | Benches; not published. |
| `lunaris-conformance` | internal | Cross-backend test harness. |
| `lunaris-codegen` | internal | Build-time codegen. |
| `lunaris-recipes` | internal | Pre-1.0 recipe surface still churning. |
| `lunaris-server` | internal | HTTP server binary (Helm chart in v0.3). |
| `lunaris-py` | PyPI | Built+published via `maturin publish`. |
| `lunaris-ts` | npm | Built+published via `napi prepublish && npm publish`. |
| `xtask` | internal | Build automation. |

Order matters — every crate must publish AFTER its dependencies are
indexed by crates.io (eventual consistency ~minutes). The "TL;DR" above
publishes in topological order.

## 4. Multi-platform wheels + .node binaries

Python wheels target the matrix from `.github/workflows/python-prebuild.yml`:
cp311 / cp312 across linux-x86_64, linux-aarch64, macos-universal2,
windows-x86_64. TypeScript `.node` binaries target the matching set
via `napi prepublish`.

Both flows trigger automatically on the tag push (`v0.2.*`). The
`maturin publish` and `npm publish` commands above are the local-fallback
recipe if CI doesn't run.

## 5. Post-release verification

After all artifacts are public:

- `cargo install lunaris --locked` from a fresh clone of `examples/quickstart-rs/`
- `pip install lunaris==0.2.1 && python examples/quickstart-py/quickstart.py`
- `npm install @pilotspace/lunaris@0.2.1 && cd examples/quickstart-ts && npm start`

Each must reach "ingested episode at lsn=..." on a clean machine.

## 6. Rollback procedure

Crates.io publishes are irreversible (`cargo yank` only flags the
version; it stays installed for existing lockfiles). PyPI and npm
permit `yank` and `deprecate` respectively.

If a breaking bug ships:

1. `cargo yank --version 0.2.1 <crate>` for every published crate in the set.
2. `pip-yank` equivalent: open a PyPI ticket (no CLI for yank by default;
   use the project page Web UI).
3. `npm deprecate lunaris@0.2.1 "see issue #XYZ"`.
4. Ship `0.2.2` with the fix and a follow-up CHANGELOG entry referencing
   the yank.

The repo's `v0.2.1` tag stays — the audit trail is more important than
the published artifact disappearing.

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
