# Production ship plan — Lunaris 0.8.0

**Status:** proposed · **Written:** 2026-08-27
**Companion:** [`2026-08-27-skill-cli-memory-surface.md`](./2026-08-27-skill-cli-memory-surface.md)
**Parent:** [`2026-08-21-ship-plan.md`](./2026-08-21-ship-plan.md) (98/110 done)

---

## 1. What "production" means here — two gates, not one

| gate | claim it defends | measured by |
|---|---|---|
| **P — the product works for strangers** | "install it and get a first recall" | published artifacts, clean machine, no source build |
| **M — the product works for us** | "it stores real agent memories" | the census: curated memories exist and get used |

**M gates P, and that ordering is the whole point of this document.**
Lunaris is sold as an *agent memory engine*. Its own production store holds
**0 real curated memories across 303,874 episodes**. Launching P without M
publishes a retrieval benchmark and calls it a memory system — the one claim a
competitor can falsify in an afternoon by asking what's in our own store.

---

## 2. Current state — verified 2026-08-27, not assumed

### Shipped and working

| | |
|---|---|
| workspace version | **0.7.1**, tag `v0.7.1` |
| crates.io | `lunaris-memory` / `lunaris-core` / `lunaris-retrieve` @ **0.7.1** |
| npm | `@pilotspace/lunaris` @ **0.7.1** |
| PyPI | `lunaris` @ **0.7.1** |
| release tooling | `scripts/release-preflight.sh` (10-step gate), `scripts/release/rollback-drill.sh`, `scripts/bump-version.sh` |
| release workflows | `cli-release.yml`, `crates-publish.yml`, `integrations-publish.yml` |
| ops docs | `backup-restore` · `capacity` · `external-moon` · `observability` · `slo` · `runbooks` |
| backup drill | RPO=0 / RTO<1s, measured |

### Quality, from a real CI verdict — not a local run

Run `32506983470`, all 10 jobs green, N=40 two-arm ratchet:

| arm | J (any-gold) | wrong |
|---|---|---|
| `fast` (rerank OFF, shipped default) | **97.5%** | `134` |
| `quality` (rerank ON) | **100.0%** | — |

Zero errored questions on either arm. `quality` answers exactly the one
question `fast` misses, so the cross-encoder buys one in forty — attributable,
not noise. `fast` reproduced the Darwin-arm64 baseline **exactly**, same single
miss, on ubuntu x86: third consecutive cross-platform match.

### The gap

| | |
|---|---|
| episodes in the live store | 303,874 |
| **real curated memories** | **0** (the only 5 non-hook sources are test fixtures) |
| distinct scopes | **542** vs Moon's `max_scopes_recommended: 512` |
| running contextd | predates W4.4 — the telemetry demotion **is not live** |

---

## 3. What is actually left

### 3a. Owner-blocked — I cannot move these

| item | needs |
|---|---|
| **W0.1** | Re-cut Moon release assets, make ghcr public. **No longer a ship-blocker** (downgraded 2026-08-25 — `install-moon.sh` builds from the public source tag). Buys install *speed*. Scope is wider than filed: v0.8.5, v0.8.6 AND v0.8.7 all publish 0 assets, so Moon's release workflow has been broken across three releases. |
| **W1.3** | Branch protection on `main` (ratchet + integration + conformance required). GitHub admin. |
| **W2.14** | Tombstone the candle-era crates on crates.io. Deleted crates get tombstones, never yanks. |
| **W3.3 / S5** | LME N=125 A/B, both operating points. Needs `MINIMAX_API_KEY` — owner-handled, never logged. |

### 3b. Blocking gate M — the memory-surface milestone

The companion plan. ~7 build days + ~1 week elapsed dogfood. **This is the
critical path**, because nothing else on this list changes the fact that the
store is empty of curated memories.

### 3c. Correctness, ranked

| rank | item | why it matters | size |
|---|---|---|---|
| **P1** | **R5** — `filtered_navigate_never_leaks_foreign_source_moon` fails on ANY live Moon; `grep 'p lunaris-retrieve' .github/workflows/*.yml` returns **nothing** | a shipped API returns `[]`; the crate has never been CI'd; the filter-leak assertion after the discriminator has never executed | M |
| **P1** | **F26** — a KNN prefilter Moon cannot parse degrades to **NO FILTER** instead of erroring | silent over-return; Moon-side, needs a Moon PR | M |
| **P2** | **F3** · **F35** | both instrumented in Wave 6, neither fixed; regressions are now *detected*, so they don't block | S |
| **P2** | **R3** | bound the audit topic — Moon MQ has no `TRIM`/`MAXLEN`; a pilotspace/moon ask, not a Lunaris change | S |
| **P3** | **R4** | no TTL on working memory, scratchpad, activation ledger. Filed **UNMEASURED** — measure before writing eviction code | M |

**Severity note.** R5 and F26 are correctness bugs, **not isolation breaches**.
Scope is baked into the FT index *name* (`lunaris_{scope}_{kind}_idx`), not a
filter predicate, so a dropped filter over-returns *within* a scope and can
never cross tenants. `recipe_reads_are_scope_bound.rs` covers the cross-scope
case separately.

### 3d. Stale in the parent plan

**F22 is FIXED** on main (`6b90629` RED / `fc81fd8` GREEN / `abf4345` sweep):
write-side guard in `atomic.rs`, `repair.rs` backfill, `repair_vectors` op
through protocol → MCP → CLI. The checkbox is stale, not the code.

---

## 4. Sequence

```
NOW ──┬─ contextd redeploy            (15 min, unblocks W4.4 in production)
      │
      ├─ M: memory-surface milestone  (~7d build + ~1wk dogfood)  ◀── CRITICAL PATH
      │     Phase 0 instruments → 1 scope → 2 CLI → 3 skills
      │     → 4 read loop → 5 dogfood → 6 MCP retirement (G6+G9)
      │
      ├─ R5 + wire -p lunaris-retrieve into integration.yml  (parallel, M)
      │
      └─ F26 → pilotspace/moon PR                            (parallel, M)

                          ▼  M green (census proves curated memories exist and are used)

0.8.0 RELEASE ─┬─ release-preflight.sh (10 gates)
               ├─ bump-version.sh → every version surface
               ├─ tag v0.8.0 → crates-publish / integrations-publish / cli-release
               └─ rollback-drill.sh rehearsed before, not after
```

R5 and F26 run **parallel** to the memory milestone — different crates, no
shared files, and neither gates M.

---

## 5. Release mechanics

1. `scripts/release-preflight.sh` — clean tree · fmt · clippy `--workspace
   --all-targets -D warnings` · build · test (excluding `lunaris-py` /
   `lunaris-ts`) · doc · `cargo deny` · publish dry-run · manifest hygiene ·
   version parity.
2. `scripts/bump-version.sh` — **and verify every surface**: the TS
   `package-lock.json` and `examples/*/Cargo.lock` were both missing from it
   historically.
3. Tag `v0.8.0`. **CI does not run on tags** — the board must be green on
   `main` *before* tagging.
4. Publish order is forced by the dependency graph: `lunaris-core` first, then
   the rest.
5. `lunaris-cli`, `lunaris-mcp`, `lunaris-hook` are `publish = false`
   (path dep on `lunaris-memory-service` → `vendor/moon`; the `moon` name on
   crates.io belongs to a third party). They ship as **GitHub Release
   binaries** via `cli-release.yml`.
6. Rehearse `scripts/release/rollback-drill.sh` before the tag, not after.

---

## 6. Go / no-go

| # | gate | state |
|---|---|---|
| 1 | two-arm recall ratchet green, both operating points labelled | **DONE** — 97.5% / 100.0% |
| 2 | backup + restore drill | **DONE** — RPO=0 / RTO<1s |
| 3 | ops docs complete | **DONE** |
| 4 | distribution live on all three registries | **DONE** — 0.7.1 |
| 5 | **M — curated memories exist and are used** | **NOT MET — reclassified, see below** |
| 6 | R5 fixed AND `lunaris-retrieve` gated in CI | **DONE** — PR #217: fixture corrected, `integration.yml` runs the crate against a live Moon, `assert-strict-fires.sh` gained `retrieve-nav` |
| 7 | F26 resolved or documented as a known Moon limitation | **DONE** — documented; the reverse-ratchet `f26_workaround_still_needed.rs` fires when the vendored Moon gains the parser fix, and moon#648 closed COMPLETED 2026-08-23 |
| 8 | `release-preflight.sh` green on the release commit | **DONE** — 10/10 at 0.7.1, re-run at 0.8.0 |
| 9 | branch protection (W1.3) | **OWNER** |
| 10 | LME N=125 A/B republished (W3.3) | **OWNER** |
| 11 | `lunaris-integrations` publishable to PyPI | **OWNER — new** |

### Gate 5 is not met, and 0.8.0 ships anyway

This table originally said "ship when 5, 6, 7, 8 are green". Gate 5 asks
whether curated memories exist and are used; on the production store today,
across 303,874 episodes, there are zero. Closing it means building the
agent-facing skill that decides *when* to call `memory.remember`, which is the
0.9.0 memory-surface milestone — a milestone, not a release blocker.

Shipping 0.8.0 with gate 5 open is a deliberate call, and it is only defensible
because the release does not claim otherwise. `CHANGELOG.md`'s 0.8.0 entry
opens with "What this release does *not* claim" and states the zero-curated-
memories measurement outright. 0.8.0 ships the *capability* (`memory.remember`,
`memory.profile`, retention, a readable audit trail); 0.9.0 ships the
*behaviour*. What would be wrong is shipping the capability and describing it
as the behaviour.

### Gate 11 — `lunaris-integrations` is not on PyPI

Found while walking the runbook: the name returns HTTP 404, identical to an
invented package name, while `lunaris` and `lunaris-mcp` return 200. Its
publish workflow exists and is correct, but its first run is gated on
`secrets.MATURIN_PYPI_TOKEN` being scoped to the `lunaris` project — it will
403 until the owner widens it or creates a `lunaris-integrations` PyPI project
with a scoped token under the same secret name. `docs/POSITIONING.md` and the
book's why-lunaris page both tell readers to `pip install
lunaris-integrations[langgraph]`, so the claim is false for strangers until
this is resolved. The 0.8.0 CHANGELOG says so explicitly, so the release is
honest either way; resolving the token makes the docs true instead.

This is an owner action on a secret. It does not block the Rust/npm/PyPI
`lunaris` artifacts.

**Ship when 6, 7 and 8 are green** — they are. 9, 10 and 11 are owner actions
that should land before the announcement but do not block the artifacts.

---

## 7. Honest risks

- **M depends on the agent remembering to call the skill.** Logged when the
  approach was chosen (2026-08-20). G6 is the only detector that isn't wishful.
- **542 > 512 scopes.** The memory plan stops new proliferation for durable
  writes; it does not prune. Recall p99 degradation above Moon's soft limit is
  *documented by Moon*, **not measured here**.
- **R5 has been failing for an unknown period** because no workflow ever ran
  the crate and the suite skips itself without `MOON_URL`. Fixing it without
  wiring CI leaves the next regression equally invisible.
- **A tag with no CI run.** Workflows do not fire on tags; a red `main` tagged
  anyway ships red. Gate 8 exists for exactly this.
