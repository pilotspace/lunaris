# Lunaris benchmark evidence

**The rule: a published number without a committed raw artifact is not
publishable.**

Not "should be avoided" — *not publishable*. If you cannot point a
stranger at a file in this repository that a re-run would reproduce, the
number does not go in the README, the book, a release note, a changelog,
a slide, or a comparison table. Retract it instead. Softening it
("roughly", "~", "on our hardware") is worse than deleting it, because a
softened number still gets quoted.

This is not a style preference. It is the entire competitive claim.
Every agent-memory project publishes a benchmark table; the ones worth
believing ship the harness that produced it. Ours does. That advantage
survives exactly as long as our own numbers pass the test we apply to
theirs.

---

## The four things every published number must carry

1. **Its operating point** — `fast` or `quality`. See
   [`operating-points.md`](operating-points.md). An unlabelled number is
   a defect, not a shortcut.
2. **Its configuration** — corpus size, k, graph on/off, reader model,
   judge model, dataset split, machine class.
3. **Its N and its error bar** — or an explicit statement that the
   sample is too small to carry a delta (see
   [`measurement-noise.md`](measurement-noise.md)).
4. **A committed raw artifact** — the envelope the harness emitted, in
   this directory, alongside the commit SHA it was measured at.

---

## Raw artifact directories

| Directory | Benchmark | Status |
|---|---|---|
| [`ga2b-raw/`](ga2b-raw/) | GA-2b recall-latency capacity envelope | **populated** — the reference implementation of this convention |
| [`lme-raw/`](lme-raw/README.md) | LongMemEval-S | **scaffolded, empty** — awaiting the W3.3 re-run |
| [`pm-raw/`](pm-raw/README.md) | PersonaMem | **scaffolded, empty** — awaiting a re-run with artifact retention |

An empty directory with a README is the honest state. It says "we know
what has to be here and it is not here yet", which is a different and
much better position than a populated results table with nothing behind
it.

Envelopes are produced by
[`scripts/bench/publish_raw.py`](../../scripts/bench/publish_raw.py) so
that the required fields cannot be forgotten. See
[`lme-raw/README.md`](lme-raw/README.md) for the envelope schema.

---

## Documents

| Document | What it establishes |
|---|---|
| [`operating-points.md`](operating-points.md) | `fast` vs `quality`; which benchmark ran at which; the labelling rule |
| [`measurement-noise.md`](measurement-noise.md) | The ±5-point judge/generation noise floor (evidenced), and the reader-drift figure that was retired for lack of one |
| [`lift-and-token-reduction-design.md`](lift-and-token-reduction-design.md) | Design (not results) for the headline lift + token-reduction measurement |
| [`../operations/capacity.md`](../operations/capacity.md) | The measured 100k-doc latency envelope, both operating points |
| [`v0.7-longmemeval-jscore-validation.md`](v0.7-longmemeval-jscore-validation.md) | **Retracted** — kept as the dated record of an unreproducible claim |
| [`v0.2.x/README.md`](v0.2.x/README.md) | **Superseded** |

---

## Competitor figures

**None of the cross-system rows anywhere in this repository are
head-to-head comparisons.** Every published agent-memory benchmark uses
its own reader model, its own dataset split, its own prompt, and often
its own scoring rubric. A number from another project's README tells you
what that project measured under its own conditions. It does not tell you
what would happen if you swapped memory systems and held everything else
fixed. Only the [lift measurement](lift-and-token-reduction-design.md)
does that, and only within its own harness.

Rules for citing one:

- **A URL to a primary source, or the row does not exist.** A vendor
  README, a vendor research page, or a paper — not a summary, not a blog
  aggregator, not our own notes.
- **State the benchmark the number is actually from.** See the Mem0 case
  below for why this is not a pedantic requirement.
- **Never source a public claim from `.planning/`.** That submodule is
  private; a reader cannot open it. A claim whose only support is
  `.planning/` is an uncited claim.

### Current rows

| Project | Figure we cite | Source |
|---|---|---|
| TencentDB-Agent-Memory | PersonaMem 76% with memory / 48% without (split and reader **unstated by them**) | [github.com/TencentCloud/TencentDB-Agent-Memory](https://github.com/TencentCloud/TencentDB-Agent-Memory) |

### Removed 2026-08-21 (W3.9)

- **"Zep 90.2%" on LongMemEval — removed.** It appeared in the README
  and in the LongMemEval validation doc with **no citation anywhere in
  the repository**, and with a reader model (`GPT-4o`) asserted in the
  README that no source in the repo supports. The primary Zep paper —
  *Zep: A Temporal Knowledge Graph Architecture for Agent Memory*,
  [arXiv:2501.13956](https://arxiv.org/abs/2501.13956) — states
  "accuracy improvements of up to 18.5%" on LongMemEval in its abstract
  and does not state an absolute 90.2%. We could not identify a primary
  source that states that figure together with the reader model it used,
  so we do not restate it. Readers who want Zep's own numbers should go
  to Zep.
- **"Mem0 66–68%" on LongMemEval — removed, and it was the wrong
  benchmark.** This repository's own competitive note records 66.9% (and
  68.4% for the graph variant) as Mem0's **LoCoMo** LLM-as-a-Judge
  figures — `docs/competitive/mem0-gap-analysis.md:118`, "26% relative
  LLM-as-a-Judge improvement over OpenAI memory on LoCoMo (66.9% vs
  52.9%)". Those numbers were printed in a **LongMemEval-S** table in the
  README and in the validation doc. That is a mis-attribution, not a
  citation gap, and it is exactly the kind of error an uncited row hides.
  Mem0 publishes its own benchmark results at
  [mem0.ai/research](https://mem0.ai/research); we point there rather
  than restate figures we have not verified against a primary source.

Both rows are struck through rather than deleted in
[`v0.7-longmemeval-jscore-validation.md`](v0.7-longmemeval-jscore-validation.md),
so the record of what was claimed stays legible.
