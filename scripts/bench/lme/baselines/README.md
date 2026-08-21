# CI recall-ratchet baselines

The checked-in numbers `anygold_gate.sh` ratchets against. A baseline is
not a result — it is a **floor**, and a floor that nothing can fall
through gates nothing.

This file records two defects found in the 2026-08-21 evidence audit
(W3.7), the decisions taken, and the exact state of each baseline file.

---

## Defect (a) — the ratchet measured the path we do not ship

`ci-anygold.json` carries the config signature
`…|rerank=1|graph=0|…`. `rerank=1` is the **quality** operating point.
The **shipped default is `fast`** — rerank is opt-in, off unless
`LUNARIS_RECALL_RERANK` is truthy at handle construction
(`crates/lunaris/src/recall_rerank.rs`). So the configuration every user
gets by default had **no recall-quality gate at all**, while CI spent its
budget guarding an opt-in path.

The original choice was not careless — `recall-ratchet.yml` states the
reasoning: "rerank INCLUDED, because disabling it demonstrably flips
outcomes". That is a good argument for gating `quality`. It is not an
argument for leaving `fast` ungated.

### Decision: **ratchet both, with `fast` as the primary gate.**

Rejected alternatives:

- *Move the ratchet to `fast` only.* Cheapest, and it does cover the
  shipped default — but the reranker is a real shipped component with
  real published numbers behind it (PersonaMem's 75.0% is a `quality`
  measurement, as is every LongMemEval figure this project has ever
  produced). Ungating it means the only path our quality claims rest on
  is unguarded. Swapping one blind spot for the other is not progress.
- *Keep `quality` only.* This is the status quo, and it is the defect.
- *Ratchet both.* Chosen. Under the two-operating-points decision
  (`docs/benchmarks/operating-points.md`) both points are published, and
  a published number without a gate rots. `fast` is designated
  **primary**: if budget ever forces a cut, `quality` is what gets
  demoted to the weekly cron, never `fast`.

---

## Defect (b) — the gate could not fail on anything smaller than a disaster

Two independent sensitivity problems, both on `questions/offsets16.tsv`.

### (b1) N is too small for the tolerance

`tally.py` fails when `hits < baseline_hits − tolerance`, i.e. on a drop
of **`tolerance + 1` questions**. So:

```
smallest detectable regression  =  (tolerance + 1) / N
```

| N | tolerance | fail floor | smallest regression caught |
|---|---|---|---|
| **16 (shipped)** | 1 | 14/16 | **2/16 = 12.5 points** |
| 16 | 0 | 15/16 | 6.25 points (but see below) |
| **40 (proposed)** | 1 | *baseline* − 1 | **2/40 = 5.0 points** |
| 60 | 2 | *baseline* − 2 | 5.0 points |
| 125 | 1 | *baseline* − 1 | 1.6 points |

A 5-point retrieval regression on the N=16 gate is invisible. That is
not a hypothetical bar: 5 points is the scale of the deltas this project
routinely argues about.

**Why not just set tolerance 0 at N=16?** Because the tolerance is not
slack, it is a correctness allowance. `tally.py`'s own comment records
why: "an any-gold flip needs only one borderline rank to cross the
session-capping boundary, and cross-platform float math (the baseline box
vs the CI runner) can plausibly move one." Removing it converts a
known-benign platform difference into a red gate, and a gate that cries
wolf gets disabled. Keep `tolerance = 1` and buy sensitivity with N.

**Proposal: N = 40, tolerance = 1 → exactly 5.0 points.**
`N ≥ 20 × (tolerance + 1)` is the general form; 40 is the smallest N
meeting the stated target.

### (b2) Four of six categories were never measured

`offsets16.tsv` contains only `multi-session` and
`single-session-preference`. `temporal-reasoning` (26% of the N=125
manifest), `knowledge-update`, `single-session-user` and
`single-session-assistant` are **absent**. A regression confined to any
of them is invisible **at any N** on that manifest. This defect is
independent of (b1) and is not fixed by raising N alone.

`questions/offsets40.tsv` (added with this note) covers all six,
proportionally to the N=125 manifest, by a deterministic derivation with
no RNG — re-running the derivation reproduces the file byte for byte.

### Runner-cost arithmetic

Reference measurement, from `recall-ratchet.yml`: **~125 s/question** on
the reference box at the full production retrieval config with rerank
included.

**Measured 2026-08-21** (N=40 per arm, same box, one process per
question): `fast` median **141.5 s**, `quality` median **163.5 s** — the
cross-encoder costs about **15%**, not a factor. Reported as a ratio
rather than as a correction to the 125 s figure above, because that
figure came from the CI reference runner and this one did not; the ratio
transfers across hardware and the absolute seconds do not.

Concurrent heavy disk I/O ran during part of the `quality` arm, so the
timings were checked for contamination rather than assumed clean: the six
questions overlapping that window came in *faster* (median 145 s) than
the rest (163.5 s), and excluding them moves the ratio 1.145 -> 1.155.
No correction applied.

So the guidance stands, now for a measured reason: **budget both arms at
the same per-question cost.** A 15% saving does not change the shard
layout, and assuming a larger one would be planning against a number
nobody measured.

| Layout | q/shard | measure time/shard | jobs | fits the 70-min job timeout? |
|---|---|---|---|---|
| today: N=16, 4 shards, 1 point | 4 | ~8.3 min | 4 | yes, large margin |
| **N=40, 4 shards, 2 points** | **10** | **~20.8 min** | **8** | **yes** (+ build, ~30–35 min total) |
| N=40, 8 shards, 2 points | 5 | ~10.4 min | 16 | yes, but 16 cache restores + 16 builds |
| N=125, 4 shards, 1 point | 31 | ~65 min | 4 | **no** — exceeds the timeout |

Chosen layout: **4 shards × 2 operating points = 8 measure jobs at
10 questions each.** Wall clock stays in the same class as today
(~35 min vs ~40 min budget); the extra cost is 4 additional jobs whose
build step is a `Swatinem/rust-cache` restore against the shared
`recall-ratchet-cpu` key, not a cold compile. Sharding stays round-robin
so category stratification survives the split.

The `fast` arm additionally does **not** need the reranker GGUF
(`anygold_gate.sh` only requires it when `RERANK=1`), so it downloads and
caches less.

---

## The baseline files

| File | Operating point | Manifest | Status |
|---|---|---|---|
| `ci-anygold.json` | `quality` (rerank ON) | `offsets16.tsv`, N=16 | **BLESSED 2026-08-17** — the live gate. Fail floor 14/16 ≈ 12.5 points. Legacy; keep until the N=40 pair is blessed, then retire. |
| `ci-anygold-fast-n40.json` | `fast` (rerank OFF) | `offsets40.tsv`, N=40 | **BLESSED 2026-08-21** — 39/40. Fail floor 38/40 = 5.0 points. |
| `ci-anygold-quality-n40.json` | `quality` (rerank ON) | `offsets40.tsv`, N=40 | **BLESSED 2026-08-21** — 40/40. Fail floor 39/40 = 5.0 points. |

Both N=40 files were withheld from this directory until they had been
measured. A baseline is a measurement; committing a placeholder with a
plausible `hits` value would be inventing a number, and inventing the
*floor* is the worst place to do it, because it silently defines what
counts as a regression. They were produced by the blessing commands below
on a machine that actually ran the questions.

### What the measurement said (2026-08-21, N=40 each, judge-free)

| Arm | hits | multi-sess | temporal | knowl-upd | ss-user | ss-asst | ss-pref |
|---|---|---|---|---|---|---|---|
| `fast` (rerank OFF) | **39/40** | 11/11 | 11/11 | 6/6 | 6/6 | 4/4 | 1/2 |
| `quality` (rerank ON) | **40/40** | 11/11 | 11/11 | 6/6 | 6/6 | 4/4 | 2/2 |

The arms differ by exactly one question: **q134**
(`single-session-preference`) misses under `fast` and hits under
`quality`. That is the whole measured value of the cross-encoder on this
manifest, and it is a real effect rather than a tie-break artifact:
`pool` is per retrieval arm, so the fused vector+BM25 set holds up to
`2 x pool = 80` candidates, `hybrid_rerank_top_in` widens the
cross-encoder window to exactly that 80, and `topk=60` then keeps 60.
Reranking discards 20 of 80, so reordering genuinely changes set
membership and any-gold can see it.

**Closing defect (b2) exposed no hidden weakness.** The four categories
`offsets16.tsv` never covered — `temporal-reasoning`, `knowledge-update`,
`single-session-user`, `single-session-assistant` — score perfectly on
both arms, and the only miss anywhere sits in a category the old manifest
*did* cover. Nothing about this run argues the widened manifest was
needed. The argument for it is unchanged and still forward-looking: a
future regression confined to 27.5% of the N=125 manifest would have been
invisible at any N, and this run establishes that those categories start
from a clean floor rather than a silently-broken one.

`ci-anygold.json` keeps its filename — `crates/lunaris-bench/tests/eval_workflow_guard.rs`
asserts on that exact path, and renaming it would break the guard and the
workflow in the same commit.

### Blessing

No API key is needed. Any-gold is judge-free: it needs only the embedder
GGUF (plus the reranker GGUF for the `quality` arm) and the public
LongMemEval-S dataset.

```sh
cargo build --release -p lunaris-bench --bin lunaris-evals --features llamacpp

# fast path (shipped default) — reranker GGUF not required
OFFSETS_FILE=scripts/bench/lme/questions/offsets40.tsv \
RERANK=0 DIR=target/lme/anygold-fast MOON_PORT=6455 \
LME_MOON_BIN=<moon-binary> \
  scripts/bench/lme/anygold_gate.sh \
    --write-baseline scripts/bench/lme/baselines/ci-anygold-fast-n40.json

# quality path (opt-in rerank)
OFFSETS_FILE=scripts/bench/lme/questions/offsets40.tsv \
RERANK=1 DIR=target/lme/anygold-quality MOON_PORT=6456 \
LME_MOON_BIN=<moon-binary> \
  scripts/bench/lme/anygold_gate.sh \
    --write-baseline scripts/bench/lme/baselines/ci-anygold-quality-n40.json
```

Note the distinct `DIR` per arm: `anygold_gate.sh` fingerprints its
artifact directory and refuses (exit 3) to mix two configs in one.
`MOON_PORT` must be a port nothing answers on — the gate flushes between
questions and refuses any Moon it did not start. **Never 6379 / 6380 /
6381.**

After blessing, publish the run's envelope too:

```sh
python3 scripts/bench/publish_raw.py --benchmark lme --anygold \
  --dir target/lme/anygold-fast --expected 40 --operating-point fast \
  --arm ci-ratchet \
  --out docs/benchmarks/lme-raw/$(date -u +%F)-anygold-n40-fast.json
```

### Rollout order — this matters

The workflow change **must land after** the two N=40 baselines are
committed. `anygold_gate.sh` exits 2 on an unreadable `--baseline`, and
`tally.py` exits 6 on a manifest-total mismatch, so pointing CI at files
that do not exist turns the gate red on every push.

1. Land `offsets40.tsv` + this note (no behaviour change; the live gate
   still runs N=16 `quality`).
2. Bless both N=40 baselines on a GGUF-staged machine; commit them with
   their raw envelopes.
3. Apply the `recall-ratchet.yml` change (owned by the workflow agent).
4. Retire `ci-anygold.json` and `offsets16.tsv`-as-gate in a follow-up,
   once the N=40 pair has run green at least once on `main`.

---

## What this gate is not

- **Not a J-score.** Any-gold measures "was a gold-evidence session
  present in the capped reader context". It says nothing about whether
  the reader then answered correctly. Never report a J delta from it.
- **Not a headline.** N=40 gives a wide confidence interval on an
  absolute score. It is a regression floor, not a published number.
- **Not noise-bounded the way J is.** Any-gold is deterministic, which is
  the whole reason it can carry a 2-question tolerance where a J-score
  cannot (`docs/benchmarks/measurement-noise.md`).
