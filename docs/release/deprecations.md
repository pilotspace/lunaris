# crates.io deprecations — owner action list

**Status:** open. **Audited:** 2026-08-15 against the live crates.io API.

Everything on this page is an **external registry action**. It cannot be done
from this repository, from CI, or by a `cargo publish` run: each command needs a
crates.io API token belonging to a crate **owner**. This document exists so the
owner can execute the list without re-deriving the analysis.

Nothing here has been executed. Ordering is not significant except where noted.

---

## Audit snapshot

| Crate | crates.io | Latest there | Latest we build | Verdict |
|---|---|---|---|---|
| `lunaris-memory` (umbrella) | yes | `0.6.0-rc.1` | `0.6.0-rc.2` | current |
| `lunaris-core` | yes | `0.6.0-rc.1` | `0.6.0-rc.2` | current |
| `lunaris-llamacpp` | yes | `0.6.0-rc.1` | `0.6.0-rc.2` | current |
| `lunaris-embed-remote` | yes | `0.6.0-rc.1` | `0.6.0-rc.2` | current |
| **`lunaris-embed-native`** | yes | **`0.5.0`** | *deleted* | **stale — deprecate** |
| **`lunaris-rerank-native`** | yes | **`0.5.0`** | *deleted* | **stale — deprecate** |
| `lunaris-hook` | no | — | `publish = false` | nothing to do |
| `lunaris-mcp` | no | — | `publish = false` | nothing to do |
| `lunaris-server` | no | — | `publish = false` | nothing to do |
| `lunaris-memory-service` | no | — | `publish = false` | nothing to do |
| **`moondb`** | yes | **`0.2.1`** (2026-06-13) | vendored, declares `0.2.1` | **needs a release — see §2** |

No version of any lunaris crate is currently yanked.

---

## 1. `lunaris-embed-native` / `lunaris-rerank-native` — stale candle-era crates

### What they are

Both are candle-backed inference crates deleted from the workspace by the
**v0.6 llama.cpp-only cutover** (ADR 2026-07-10, see
`docs/migration/0.5-to-0.6-llamacpp-only.md`). Their replacement is the single
crate `lunaris-llamacpp`. They were last published at `0.5.0` on 2026-06-16 and
will never receive another functional release, so crates.io still presents them
as live components of Lunaris.

### Recommendation: README-deprecation. **Do NOT `cargo yank`.**

This is not a style preference — yanking these two versions breaks a release
line that is still on the registry.

`lunaris-memory 0.5.0` declares both as **non-optional, normal** dependencies:

```
$ curl -s https://crates.io/api/v1/crates/lunaris-memory/0.5.0/dependencies
lunaris-embed-native   ^0.5.0   kind=normal   optional=false
lunaris-rerank-native  ^0.5.0   kind=normal   optional=false
```

`cargo yank` does not delete a version; it removes it from **new** dependency
resolution while leaving existing `Cargo.lock` files working. So yanking
`lunaris-embed-native 0.5.0` and `lunaris-rerank-native 0.5.0` would leave
`lunaris-memory 0.5.0` on the registry but **unresolvable from a fresh
`cargo add lunaris-memory@0.5.0`** — a hard break for anyone still adopting or
re-locking the 0.5 line, in exchange for no safety benefit. These crates are
obsolete, not unsound; there is no CVE and no data-loss bug motivating a yank.

Yanking would only become correct if `lunaris-memory 0.5.0` were yanked at the
same time, which is a much larger decision (it retires the whole 0.5 line) and
is explicitly **not** recommended here.

### The mechanism to use instead

crates.io has no "deprecated" flag. The supported way to mark a crate dead is to
publish one final tombstone patch release whose README — which is what the
crates.io landing page renders for the **latest** version — says so.

Both crates were deleted from the tree, so the tombstone has to be a fresh
minimal crate, not a rebuild of the old one. Per crate:

```bash
cargo new --lib /tmp/lunaris-embed-native-tombstone
```

`Cargo.toml`:

```toml
[package]
name        = "lunaris-embed-native"
version     = "0.5.1"                # patch bump over the last real release
edition     = "2021"
license     = "Apache-2.0"
repository  = "https://github.com/pilotspace/lunaris"
readme      = "README.md"
description = "DEPRECATED — superseded by `lunaris-llamacpp`. See the repository migration note."
```

`src/lib.rs`:

```rust
//! **DEPRECATED.** The candle inference backend was removed in Lunaris v0.6
//! (ADR 2026-07-10). Use [`lunaris-llamacpp`](https://crates.io/crates/lunaris-llamacpp).
//!
//! Migration: <https://github.com/pilotspace/lunaris/blob/main/docs/migration/0.5-to-0.6-llamacpp-only.md>
#![deprecated(note = "superseded by `lunaris-llamacpp` in Lunaris v0.6; this crate is no longer maintained")]
```

`README.md` — the part users actually see:

```markdown
# lunaris-embed-native — DEPRECATED

This crate was the candle-backed embedder for Lunaris 0.3–0.5. It was removed
in v0.6 (llama.cpp-only cutover, ADR 2026-07-10).

**Use [`lunaris-llamacpp`](https://crates.io/crates/lunaris-llamacpp) instead.**

Migration guide: https://github.com/pilotspace/lunaris/blob/main/docs/migration/0.5-to-0.6-llamacpp-only.md

`0.5.0` remains on crates.io and is deliberately **not** yanked: `lunaris-memory 0.5.0`
depends on it, and yanking would make that release unresolvable.
```

Then, from the tombstone directory:

```bash
cargo publish
```

Repeat verbatim for `lunaris-rerank-native` (swap the name, and say "reranker"
in the prose).

### Owner commands, for reference

```bash
# Confirm you are an owner before anything else.
cargo owner --list lunaris-embed-native
cargo owner --list lunaris-rerank-native

# Authenticate (needs a token with publish-update scope).
cargo login

# NOT RECOMMENDED — recorded only so the decision is auditable.
# This is the command deliberately NOT being run, for the reason in §1:
#   cargo yank --version 0.5.0 lunaris-embed-native
#   cargo yank --version 0.5.0 lunaris-rerank-native
# If one were ever run in error, it is reversible:
#   cargo yank --version 0.5.0 --undo lunaris-embed-native
```

### Verification after the tombstones are published

```bash
curl -s https://crates.io/api/v1/crates/lunaris-embed-native  | jq -r .crate.max_version   # expect 0.5.1
curl -s https://crates.io/api/v1/crates/lunaris-rerank-native | jq -r .crate.max_version   # expect 0.5.1
# And 0.5.0 must still resolve — this must keep working:
cargo add lunaris-memory@0.5.0 --dry-run
```

---

## 2. `moondb` — published source has silently diverged from the pinned SDK

Not a deprecation, but it belongs on the same owner action list: it is the other
crates.io-side item that blocks a clean release, and it is the reason
`.github/workflows/crates-publish.yml` now fails instead of skipping.

### The finding

The workspace pins `moon = { path = "vendor/moon/sdk/rust", version = "0.2.1",
package = "moondb" }`. The submodule is pinned at moon **`v0.8.5`**, and that
tag's `sdk/rust/Cargo.toml` *still declares* `version = "0.2.1"` — the same
version string that has been on crates.io since 2026-06-13 — while the source
underneath moved several moon releases forward.

Measured 2026-08-15, published `moondb 0.2.1` versus the pinned `v0.8.5` source:

```
$ diff -rq <published 0.2.1>/src <vendor/moon/sdk/rust>/src
Files ... cache.rs and ... cache.rs differ
Files ... client.rs and ... client.rs differ
Files ... graph.rs and ... graph.rs differ
Files ... mq.rs and ... mq.rs differ
Files ... session.rs and ... session.rs differ
Files ... temporal.rs and ... temporal.rs differ
Files ... text.rs and ... text.rs differ
Files ... vector.rs and ... vector.rs differ
Files ... workspace.rs and ... workspace.rs differ
                                     # 9 of 13 source files
```

The sharpest instance: `client.rs` in published `0.2.1` contains **zero**
occurrences of `ConnectionManager` (it is still on `MultiplexedConnection`); the
pinned `v0.8.5` source contains **seven**. The reconnect fix (moon PR #419) —
the fix for the post-flip `broken pipe` recall wedge — is therefore **absent
from every crates.io consumer of the published lunaris crates**, even though
every crate published from this repo pins `moondb = "0.2.1"`.

This went unnoticed because the publish job decided "already published" from the
version string alone and logged
`moondb 0.2.1 already on crates.io — skipping` on every run.

### Repo-side repair (already landed on this branch)

`scripts/check-vendored-moondb-parity.sh` now compares the vendored source
against the published `.crate` at the same version and fails the publish job on
divergence. The version string is no longer treated as an identity.

Consequence, stated plainly: **the next `v*` tag will fail the `crates-publish`
job** until the owner action below is done. That is the intended behaviour — the
alternative is continuing to ship crates whose pinned moondb is not the moondb
they were tested against.

### Owner action — in the `moon` repo, not this one

crates.io versions are immutable, so `0.2.1` cannot be corrected in place. The
fix is a real release:

1. In `pilotspace/moon`, bump `sdk/rust/Cargo.toml` to a version that reflects
   the accumulated change. The SDK gained public API since `0.2.1`
   (`ConnectionManager`-based reconnect among it), so `0.3.0` is the correct
   floor — not a patch bump.
2. Publish it: `cargo publish --manifest-path sdk/rust/Cargo.toml`.
3. Tag `moon` and, **in this repo**, bump both halves of the pin together:
   - the `vendor/moon` submodule -> the new tag, and
   - `Cargo.toml`'s `moon = { ..., version = "0.3.0", package = "moondb" }`.

   `crates-publish.yml`'s existing *"workspace moon pin matches vendored moondb
   version"* step already fails the job if those two drift, and the new parity
   script fails it if the source drifts. Both must be green.
4. Re-run `crates-publish` (`workflow_dispatch` works without re-tagging).

### Standing recommendation

The root cause is that the moon SDK's crate version is maintained separately
from the moon server's release tags, so a tag bump does not force an SDK
version bump. Until the moon repo gates that (an SDK-version-vs-source check in
its own CI), this repo's parity script is the only thing catching it, and it
catches it at release time rather than at submodule-bump time. Moving the check
to the submodule-bump PR in this repo would be the next improvement.

---

## Not on this list, and why

- **`lunaris-hook`** — set to `publish = false` on 2026-08-15. It is a
  Claude-Code integration **binary**, not a library, and it depends on
  `lunaris-memory-service` (`publish = false`), so `cargo publish -p
  lunaris-hook` could only ever fail. It was never published (crates.io returns
  404 for the name), so there is no registry action and nothing to deprecate —
  the manifest was simply wrong. Distribution stays: build from the repo, or the
  prebuilt binaries.
- **`lunaris-mcp`, `lunaris-server`, `lunaris-memory-service`** — all
  `publish = false`, all absent from crates.io. `lunaris-mcp` ships prebuilt
  binaries via `npx` / `uvx` instead.
- **Old `lunaris-memory` / `lunaris-core` versions (`0.2.1` … `0.5.0`)** — kept
  live on purpose. They are superseded, not broken, and yanking them would break
  existing lockfiles' ability to re-resolve for no safety gain.
- **The bare `lunaris` name on crates.io** — an unrelated third-party project.
  Not ours; nothing to do. The umbrella publishes as `lunaris-memory` and is
  imported as `lunaris` via `--rename` / `package =`.
