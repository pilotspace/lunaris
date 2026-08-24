# Production ship context — Lunaris 0.7.x

*Assembled 2026-08-24 from the live tree, the live registries and the live
CI board. Every claim below was measured today; none is carried over from
the ledger. Where a ledger entry disagreed with the measurement, the
measurement wins and the disagreement is called out.*

Ledger: [`2026-08-21-ship-plan.md`](2026-08-21-ship-plan.md) — 23 items still open
(15 `W`, 8 `F`/`S`).

---

## 1. Verdict

**Do not cut a new release yet. One already-published artifact carries a
multi-tenancy defect and must be superseded before anything else ships.**

The engine is not the problem. `main` is green on all 9 workflows at
`19ca20b`, recall holds its latency contract, and the two W4.12/W4.17
isolation fixes have landed. The problem is entirely at the distribution
boundary: what the public can actually install does not match what the
repo contains, in three different ways, and one of those ways is a data
leak.

---

## 2. What is actually installable right now

| Channel | State | Reality |
|---|---|---|
| **PyPI `lunaris`** | **live — 0.7.0, defective** | 5 platform wheels, uploaded **2026-08-18**. Predates both isolation fixes. **Its DSL cannot carry a partition key.** See §3. |
| **npm `lunaris`** | **not ours** | Belongs to an unrelated Chinese lunar-calendar/BaZi library (`discountry/lunaris`, 0.5.1). Ours is `@pilotspace/lunaris`. |
| **crates.io** | **absent** | `lunaris-memory`, `lunaris-core`, `lunaris-retrieve`, `lunaris-storage-moon` — **none published**. The names are unclaimed. |
| **Moon substrate** | **installable (source)** | v0.8.5/6/7 still ship **0 assets** and ghcr is still private — but `scripts/install-moon.sh` builds from the public source tag, so this no longer blocks. Assets would only make it *faster*. |

Two of these contradict `CLAUDE.md`, which states the public story as
"public Rust crate / `pip install lunaris` / `npm i lunaris`". Measured:
`pip install lunaris` works and installs a defective build; `npm i lunaris`
installs a stranger's calendar library; `cargo add lunaris-memory` fails —
nothing is there.

---

## 3. P0 — the published Python DSL reads across every partition

**Severity: highest open item in the project.** This is live, public, and
downloadable right now.

PyPI `lunaris` 0.7.0 exposes `Scope`, `ScopedLunaris` and `.scoped()`, so
the multi-tenancy surface *appears* present. It is not wired. The shipped
`lunaris/dsl.py` contains **one** occurrence of the string `scope`, and it
is a comment about `__slots__`. Current `main` threads `_inherit_scope`
through seven builder transitions.

Consequence: a caller who does
`engine.scoped(Scope("tenant-a")).dsl()...` builds a query with **no
partition key**, and the read is served across every tenant in the store.
The write half is scoped correctly, so the store looks right and the reads
are wrong — the same shape as W4.17, one layer up.

Reproduce:

```bash
pip download lunaris==0.7.0 --no-deps -d /tmp/l && \
  unzip -qo /tmp/l/lunaris-0.7.0-*.whl -d /tmp/l/x && \
  grep -c scope /tmp/l/x/lunaris/dsl.py     # -> 1  (a __slots__ comment)
```

The fix is already on `main` — W4.12, PR #186, merged 2026-08-24 — and has
**never been published**. Shipping it is the release, not a nice-to-have.

**This also means the llama-cpp-2 exact-pin fix (`7a8b804`) has never been
exercised.** Its whole point was that the defect is reachable only through
a registry install, and the Rust registry install has never happened.

---

## 4. Tree state — verified today

- `main` @ **`19ca20b`**, **9/9 workflows green** — CI, Integration,
  conformance-bindings, Docs, Examples, ts-prebuild, python-prebuild,
  Code Quality, Push.
- `conformance-bindings` was red for two days at `97e3837`; W4.17 cleared
  it. The F34 tourniquet is gone and the gate is real again.
- Recall: **p50 19.2–22.4 ms · p95 22.3–24.1 ms · p99 23.4–24.4 ms**
  (100k docs/scope, single-shard Moon, M4 Pro, `fast`, graph OFF, k=30,
  retrieval-only). Holds the sub-25 ms contract with ≤25 % headroom.
- Hook pipeline: **p50 12.8 ms / p99 19.8 ms** against 50/150 ms budgets.
- No publishable crate depends on an unpublishable one — the crates.io
  publish graph is clean.

---

## 5. The published-numbers contract

Non-negotiable, settled 2026-08-21: **every published number states its
operating point.**

| | `fast` | `quality` |
|---|---|---|
| Cross-encoder rerank | OFF | ON |
| Selected by | default (`LUNARIS_RECALL_RERANK` unset) | `LUNARIS_RECALL_RERANK=1` |
| Shipped default | **yes** | no |
| Recall p50 | ~20 ms | ~1301 ms |

`LUNARIS_RECALL_GRAPH` is a third axis, not a third point — `fast, graph ON`
is **p50 39.1 ms**, roughly double. Say so explicitly wherever it applies.
A number with no operating point is not publishable; retract rather than
guess.

---

## 6. Ship sequence

Four gates, in order. Each is a hard stop for the next.

**Gate 1 — supersede the defective wheel.** Cut and publish Python
0.7.1 from `main` (carries W4.12 + W4.17). Yank is *not* the move — the
defect is silent, so a yank leaves existing installs in place with no
signal; publish forward and issue an advisory naming the scoped-DSL read.
Verify by the §3 repro returning a threaded `dsl.py`, not by the build
log.

**Gate 2 — make the substrate installable. ✅ CLEARED 2026-08-25 (W0.10),
without owner action.** Lunaris no longer depends on Moon's release
pipeline. `scripts/install-moon.sh` walks a ladder — reuse an existing Moon,
else a release tarball, else a shallow clone of the public source tag plus
`cargo install --path` — and `setup-lunaris-agents.py` now *runs* it instead
of printing a command that 404s. Two assumptions had to be measured and both
were false: `cargo install --git` fails for every anonymous user (cargo
initialises Moon's private `.planning` submodule), and **the Moon binary has
no version flag at all**, so an arbitrary binary cannot be version-checked
offline. Re-cutting Moon's assets is now a speed optimisation, not a
blocker.

**Gate 3 — close the measurement layer.** F27, F31, F3, W4.18. Every
claim in §4 rests on gates that must be able to fail. F27 exists *because*
a guard was too narrow; expect the open-item count to rise here, and treat
that as the gate working.

**Gate 4 — first Rust publish.** crates.io, in dependency order, with the
llama-cpp-2 pin finally exercised against a real registry resolve.

Gate 1 and Gate 3 are mine. **Gate 2 is done.** Gate 4 waits on Gate 3.

---

## 7. Owner actions — 4 items, none of which I can do

1. **Moon release workflow is broken** (v0.8.5/6/7 = 0 assets; last good
   is v0.8.4, 2026-07-29, 48 assets). Needs a fix, not a re-run.
2. **ghcr `pilotspace/moon` is private** — anonymous pull returns 401/403.
3. **Branch protection on `main` is absent** — confirmed `404 Branch not
   protected`. Four CI gates are advisory until this lands. Four escapes
   remain: `recall-ratchet.yml:249` (`|| exit 0`), and
   `continue-on-error: true` at `perf-gates.yml:161`,
   `moon-install-smoke.yml:74`, `integration.yml:544`.
4. **`MINIMAX_API_KEY`** for the W3.3/S5 LME N=125 re-run.

Plus a naming call: bare `npm i lunaris` is unavailable. Either commit to
`@pilotspace/lunaris` in all docs, or pick a different bare name.

---

## 8. Rollback

Recall config is frozen at handle construction, so both operating points
roll back by environment, not by redeploy: unset `LUNARIS_RECALL_RERANK`
to return to `fast`. The storage substrate has RPO=0 / RTO<1 s via AOF
(`BGREWRITEAOF`, **not** `BGSAVE` — `BGSAVE` writes nothing). A bad Python
publish rolls forward only; see Gate 1.
