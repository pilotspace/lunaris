# LongMemEval raw result envelopes

**Currently empty. That is the honest state, not an oversight.**

Every LongMemEval number Lunaris has ever published was written to
`LME_RESULTS_DIR`, which defaults to `target/lme` — gitignored. When the
operator's working tree was cleaned, the evidence went with it. That is
the direct, mechanical cause of the `85.4% (427/500)` retraction
([`../v0.7-longmemeval-jscore-validation.md`](../v0.7-longmemeval-jscore-validation.md)):
there is nothing left to re-tally, so the number cannot be defended at
any price.

This directory is the fix. From now on:

> **A LongMemEval number that does not have an envelope in this directory
> is not publishable.** Not in the README, not in the book, not in a
> release note, not in a comparison table.

The convention mirrors [`../ga2b-raw/`](../ga2b-raw/), which has been
doing this correctly since GA-2b.

---

## What goes here

One JSON envelope per **arm**, per run. Not the per-question `q*.json` /
`q*.log` files — those stay in `target/`; a 125-question arm is hundreds
of files and megabytes of judge transcript. The envelope carries the
tally (including the *offset lists* for correct / wrong / ERR, so a
disagreement can be localised to specific questions) plus everything
needed to re-run.

Naming: `YYYY-MM-DD-<scale>-<arm>-<operating-point>.json`

```
2026-08-21-n125-graphoff-quality.json
2026-08-21-n125-graphon-quality.json
2026-08-21-anygold-n40-fast.json
```

Both arms of an A/B go in together, in the same commit. A single arm
published alone invites exactly the cross-day comparison that
[`../measurement-noise.md`](../measurement-noise.md) forbids.

## How to produce one

```sh
python3 scripts/bench/publish_raw.py \
    --benchmark lme \
    --dir target/lme/graphoff \
    --expected 125 \
    --operating-point quality \
    --arm graphoff \
    --out docs/benchmarks/lme-raw/2026-08-21-n125-graphoff-quality.json
```

The publisher refuses to emit an envelope for a run that is not FINAL
(exit 5), and refuses an `--operating-point` that contradicts the rerank
setting the harness actually recorded (exit 6). You cannot mislabel a
`quality` run as `fast` by hand.

## Envelope schema — `lunaris-bench-raw/1`

| Field | Meaning |
|---|---|
| `schema` | `"lunaris-bench-raw/1"` |
| `benchmark` | `"longmemeval_s"` |
| `arm` | short arm label (`graphoff`, `graphon`, `ci-ratchet`) |
| `operating_point` | **`"fast"` or `"quality"`** — see [`../operating-points.md`](../operating-points.md) |
| `config_signature` | the signature the harness computed (`SIG=` in the arm's `config.env`), so a re-run under a changed config is detectable rather than silently comparable |
| `metric` | `"j"` (LLM-judged) or `"anygold"` (judge-free retrieval) |
| `tally` | the harness's own tally output verbatim: `correct` / `wrong` / `err` offset lists, `artifacts`, `expected`, `scored`, `final`, and the score |
| `harness` | which runner, tally and publisher produced it |
| `provenance` | `commit_sha`, `working_tree_clean`, `run_date_utc`, `platform`, `artifact_dir` |
| `note` | free text — the one place a caveat belongs |

The score is never recomputed by the publisher; it comes from
`scripts/bench/lme/tally.py`.

## What is still missing, and who is blocked on what

| Needed envelope | Blocked on |
|---|---|
| N=125 A/B (`graphoff` + `graphon`), quality path — the W3.3 re-run | **owner** — `MINIMAX_API_KEY` (generation + judging + graph-arm extraction) |
| Any-gold ratchet baseline, **fast** path | a machine with the two GGUFs staged. **No API key needed** — any-gold is judge-free. See [`../../../scripts/bench/lme/baselines/README.md`](../../../scripts/bench/lme/baselines/README.md) |
| Any-gold ratchet baseline, **quality** path, at the proposed N | same |

Until those land, the correct thing to publish is nothing.
