# PersonaMem: measuring against TencentDB Agent Memory

*2026-08-17 — self-measured, fully reproducible from this repo.*

[TencentDB-Agent-Memory](https://github.com/TencentCloud/TencentDB-Agent-Memory)
publishes a headline of **76% with memory / 48% without** on
[PersonaMem](https://huggingface.co/datasets/bowen-upenn/PersonaMem), the
persona-tracking benchmark where an assistant must answer multiple-choice
questions about a user it has talked to across dozens of sessions —
current preferences, the reasons they changed, and what a good
personalized reply looks like *now*. We ran the same benchmark against
Lunaris end to end and publish everything: numbers, per-category
breakdown, harness source, and the caveats.

## Results — 32k split, 589 questions, zero errors

Measured on the **quality operating point** (rerank ON). That is *not*
Lunaris's shipped default, which is the `fast` path with rerank off —
see [operating points](https://github.com/pilotspace/lunaris/blob/main/docs/benchmarks/operating-points.md).
No PersonaMem arm has yet been run on the shipped default.

| configuration | accuracy |
|---|---|
| **Lunaris + claude-sonnet-5 reader** (single reader — the system result) | **75.0%** (442/589) |
| No-memory floor (same reader, options only) | 41.9% (247/589) |
| *TencentDB-Agent-Memory (published; split/reader unstated)* | *76% / 48%* |
| *Two-reader oracle cascade — upper bound, not a system result* | *81.8% (482/589)* |

Three claims, stated precisely:

- **The memory lift is +33.1 points** (75.0 vs 41.9 with the identical
  reader). Tencent's published lift is +28 (76 vs 48). Lunaris delivers a
  larger lift from a lower floor. Tencent's release does not state its
  split or reader model, so treat that comparison the way you should
  treat every cross-system benchmark table: context, not a controlled
  head-to-head.
- **81.8% is an oracle bound, and we will not lead with it.**
  claude-opus-5 re-answered exactly the 147 questions the Sonnet arm got
  wrong (fixing 40 of them), and *gold labels* decided which questions
  went to the second reader. A deployable cascade would need a gold-free
  routing rule. The system result is the single-reader **75.0%**.
- **75.0% is understated.** 93 of the 589 questions live in a category
  that does not measure memory at all — see below.

## What was actually measured

No shortcut path. Every conversation message is ingested through the
production write path (`CodingSessionMemory::write` → `Lunaris::ingest`:
chunk, embed, index), and every question is answered from the production
hybrid recall root — Vector ∧ BM25 → reciprocal-rank fusion →
cross-encoder rerank → top-30 — plus two retrieval features this
benchmark motivated:

- **Neighbor expansion** — each hit is rendered with ±2 surrounding
  messages, so a mid-dialogue hit arrives with the turn that prompted it.
- **Per-candidate evidence retrieval** — the store is queried with each
  answer option's own text, and the reader sees the most similar past
  messages per candidate. A recycled candidate exposes its own
  near-duplicate; a factual claim gets its receipts.

Temporal honesty is enforced twice: the store is append-only per context
and a question is asked the moment its prefix — and nothing after it —
is ingested (structural), and any retrieval hit at or past the prefix
boundary marks the question as an error rather than an answer (runtime).
Scoring is exact letter match; there is no LLM judge and therefore no
judge noise floor.

## Per-category

| category | sonnet (system) | floor | *cascade (oracle)* |
|---|---|---|---|
| reasons behind preference updates | **90.9%** | 65.7% | *97.0%* |
| full preference evolution | **82.0%** | 78.4% | *91.4%* |
| recall user-shared facts | **78.3%** | **2.3%** | *83.7%* |
| generalizing to new scenarios | **75.4%** | 10.5% | *82.5%* |
| preference-aligned recommendations | **74.5%** | 21.8% | *80.0%* |
| facts mentioned by the user | **58.8%** | 17.6% | *64.7%* |
| ⚠ suggest new ideas — *does not measure memory* | 46.2% | 52.7% | *52.7%* |

The fact-recall rows are where a memory system earns its keep: 2.3%
without memory, 78.3% with it on a single reader.

### The last row is a broken benchmark question, not a Lunaris result

In `suggest_new_ideas` the no-memory floor *matches* the best memory
configuration. We spent a long time treating that as a retrieval finding.
It is not. **The gold answer in that category is essentially always the
shortest option**: a classifier that reads nothing at all — not the
question, not the memories, not the persona — and simply picks the
shortest candidate scores **98.9%** there, against 0–15.5% in every other
category and a 25% random baseline. Gold averages 245 characters against
564 for the distractors (2.3×); every other category sits between 0.75×
and 1.17×.

The distractors are the long, persona-woven ones, so retrieved persona
content makes the *wrong* answer look better supported. That is the
entire "memory net-harms this category" effect, and it is a property of
how the dataset was constructed.

**We do not optimise against it.** A shortness preference would score
~99% here and mean nothing, while making real suggestions worse — in
production, building on what we know about a user is the *desirable*
behaviour this category punishes. The 93 questions stay in the
denominator, so our 75.0% is understated rather than inflated, and a
regression test pins the dataset property in both directions so it cannot
be re-diagnosed as a retrieval defect.

Full root-cause analysis:
[issue #141](https://github.com/pilotspace/lunaris/issues/141).

## Reproduce it

The harness ships in this repo — dataset download, incremental ingest,
both arms, per-question artifacts, and the combiner:

```sh
ARM=memory TOPK=30 NEIGHBORS=2 EVIDENCE=3 scripts/bench/pm/run_pm.sh
ARM=nomem scripts/bench/pm/run_pm.sh
```

Full numbers, config fingerprints, and the second-reader protocol:
[`scripts/bench/pm/RESULTS.md`](https://github.com/pilotspace/lunaris/blob/main/scripts/bench/pm/RESULTS.md).
Harness source:
[`crates/lunaris-bench/src/eval/personamem/`](https://github.com/pilotspace/lunaris/tree/main/crates/lunaris-bench/src/eval/personamem).
