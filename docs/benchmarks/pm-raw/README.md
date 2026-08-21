# PersonaMem raw result envelopes

**Currently empty.** The published PersonaMem numbers
([`../../../scripts/bench/pm/RESULTS.md`](../../../scripts/bench/pm/RESULTS.md))
were measured into `PM_RESULTS_DIR`, which defaults to `target/pm` —
gitignored. Those artifacts are already gone: issue #141 records that
"the prior runs' `target/pm/` artifacts are gone", which is why the
`suggest_new_ideas` question had to be answered by *re-running* six arms
rather than by re-tallying what had been measured.

That is a cheap lesson to have learned twice. This directory ends it.

> **A PersonaMem number that does not have an envelope in this directory
> is not publishable.**

Same convention as [`../ga2b-raw/`](../ga2b-raw/) and
[`../lme-raw/`](../lme-raw/README.md).

---

## What goes here

One JSON envelope per arm, per run — the tally, not the per-question
files. Because PersonaMem's headline is a *paired* comparison across
arms (memory vs floor vs full-context), the per-question verdicts matter:
without them a McNemar test cannot be computed after the fact, which is
precisely the gap issue #141 hit.

Naming: `YYYY-MM-DD-<split>-<arm>-<operating-point>.json`

```
2026-08-21-32k-memory-sonnet5-quality.json
2026-08-21-32k-nomem-floor-quality.json
2026-08-21-32k-fullctx-ceiling-quality.json
```

**Arms that will be compared must be published in the same commit.** An
arm published alone is an invitation to compare it against a number from
another day — see [`../measurement-noise.md`](../measurement-noise.md).

## How to produce one

```sh
python3 scripts/bench/publish_raw.py \
    --benchmark pm \
    --dir target/pm/32k-memory \
    --expected 37 \
    --operating-point quality \
    --arm memory-sonnet5 \
    --out docs/benchmarks/pm-raw/2026-08-21-32k-memory-sonnet5-quality.json
```

`--expected` is the **shared-context** count for the split (37 for 32k),
not the question count — that is what `scripts/bench/pm/tally.py` counts
coverage in.

Schema: `lunaris-bench-raw/1`, documented in
[`../lme-raw/README.md`](../lme-raw/README.md#envelope-schema--lunaris-bench-raw1).
The `tally` block additionally carries PersonaMem's per-category
breakdown (`by_type`) and `unparsed_replies`.

## Operating point

Every PersonaMem arm run to date used **`quality`** (rerank ON —
`LUNARIS_EVAL_PM_RERANK=1`, `scripts/bench/pm/run_pm.sh:112`), which is
**not** the shipped default. The publisher enforces the label; see
[`../operating-points.md`](../operating-points.md).

If a `fast`-path PersonaMem arm is ever run — and it should be, because
that is what users get by default — it must be published beside the
`quality` arm, not instead of it.

## Two things any future run must retain

1. **Per-question verdicts**, so paired tests (McNemar) remain possible
   without a re-run.
2. **Which messages were retrieved**, not just how many. `PM_VERDICT`
   historically recorded `hits` / `memories` / `max_hit_index` but not
   the retrieved index set, so retrieval *coverage* could not be scored
   from a finished run at all (issue #141, "Harness gap this exposed").
