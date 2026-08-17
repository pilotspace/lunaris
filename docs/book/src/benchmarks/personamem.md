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

| configuration | accuracy |
|---|---|
| **Lunaris + two-reader ensemble (oracle, see caveat)** | **81.8%** (482/589) |
| Lunaris + claude-sonnet-5 reader | **75.0%** (442/589) |
| No-memory floor (same reader, options only) | 41.9% (247/589) |
| *TencentDB-Agent-Memory (published; split/reader unstated)* | *76% / 48%* |

Two claims, stated precisely:

- **The memory lift is +33.1 points** (75.0 vs 41.9 with the identical
  reader). Tencent's published lift is +28 (76 vs 48). Lunaris delivers a
  larger lift from a lower floor.
- **The two-reader ensemble beats the published 76% by 5.8 points** —
  with a caveat we will not bury: claude-opus-5 re-answered exactly the
  147 questions the Sonnet arm got wrong (fixing 40 of them), and gold
  labels decided which questions went to the second reader. That makes
  81.8% an **upper bound on a two-reader cascade**, not a single-reader
  measurement. The clean single-reader number is 75.0%. Tencent's
  release does not state its split or reader model, so treat the
  comparison the way you should treat every cross-system benchmark
  table: context, not a controlled head-to-head.

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

| category | ensemble | sonnet | floor |
|---|---|---|---|
| reasons behind preference updates | 97.0% | 90.9% | 65.7% |
| full preference evolution | 91.4% | 82.0% | 78.4% |
| recall user-shared facts | 83.7% | 78.3% | **2.3%** |
| generalizing to new scenarios | 82.5% | 75.4% | 10.5% |
| preference-aligned recommendations | 80.0% | 74.5% | 21.8% |
| facts mentioned by the user | 64.7% | 58.8% | 17.6% |
| suggest new ideas | 52.7% | 46.2% | 52.7% |

The fact-recall rows are where a memory system earns its keep: 2.3%
without memory, 83.7% with it.

The most interesting row is the last one. In `suggest_new_ideas`, the
no-memory floor *matches* the best memory configuration — because
PersonaMem constructs those distractors **from the persona's own
history**. The gold answer is never a first-person anecdote (93/93
questions in this split) and typically *extends* an activity the user
already loves; the traps are attractive persona-flavored recycles. Naive
"be consistent with what you know" guidance scores below random here,
and so does naive "prefer novel ideas" guidance — we measured both
failure modes before landing the two-sided reader guidance documented in
the harness source.

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
