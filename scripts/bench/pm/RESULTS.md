# PersonaMem results — 32k split (2026-08-17)

Reference point: [TencentDB-Agent-Memory](https://github.com/TencentCloud/TencentDB-Agent-Memory)
publishes **PersonaMem 76% with memory / 48% without** (split and reader
unstated). Every number below names its split, its reader and its
**operating point**, per the harness's own discipline.

> **Operating point: `quality` (rerank ON).** Every PersonaMem arm ever
> run set `LUNARIS_EVAL_PM_RERANK=1` (`run_pm.sh:112`). That is **not**
> the shipped default — Lunaris ships `fast` (rerank OFF). No PersonaMem
> arm has been run on the shipped default, and one should be. See
> [`docs/benchmarks/operating-points.md`](../../../docs/benchmarks/operating-points.md).

## Headline

| configuration | accuracy |
|---|---|
| **Lunaris memory, claude-sonnet-5 reader** (the system result) | **75.0% (442/589)** |
| No-memory floor, claude-sonnet-5 reader | 41.9% (247/589) |
| *Two-reader oracle cascade — an upper bound, not a system result (§ honesty note)* | *81.8% (482/589)* |
| ~~Reader ceiling (full prefix, no retrieval, contexts {1,15,18})~~ | ~~85.3% (29/34)~~ — **see the retraction below** |

- **Memory delta: +33.1 points** (75.0 vs 41.9) — larger than Tencent's
  published +28 (76 vs 48), on a lower floor.
- Zero transport errors and zero temporal-leak trips in every run; ERR is
  never scored wrong (H3).
- **75.0% is understated**, not inflated: 93 of the 589 questions are in
  a category that does not measure memory (§ degenerate category).

## Measured configuration

Production path: `CodingSessionMemory::write` → `Lunaris::ingest`, recall via
`recall_with_degraded_check()` hybrid root (Vector ∧ BM25 → RRF →
cross-encoder rerank → top-k), graph OFF, **rerank ON (`quality`)**.
Knobs: `TOPK=30 POOL=30 NEIGHBORS=2 EVIDENCE=3`, embedder granite-embed-r2
(Ollama), reranker bge-reranker-v2-m3, readers via a local Claude-CLI
bridge. Incremental append-only ingest per shared context; a question sees
exactly `messages[..end_index]` (structural + runtime temporal-honesty
guards).

## Combined-number honesty note

The 81.8% is a **two-reader oracle ensemble**: claude-sonnet-5 answered all
589 questions; claude-opus-5 then re-answered ONLY the 147 questions Sonnet
got wrong (`LUNARIS_EVAL_PM_QIDS_FILE`), fixing 40 of them (27.2%). Because
gold labels routed which questions reached the second reader, this is an
upper bound on a two-reader cascade, not a single-reader measurement — a
deployable cascade needs a gold-free routing rule (e.g. self-reported
confidence). **The system result is 75.0%.** 81.8% may be quoted only with
"oracle upper bound" attached to it, and never as the headline.

## Retracted: the "10.3 points of retrieval headroom" reading

An earlier version of this file put the reader-ceiling arm (85.3%, 29/34)
next to the retrieval arm (75.0%, 589 questions) and read the gap as
retrieval headroom. **That reading is refuted** — see
[issue #141](https://github.com/pilotspace/lunaris/issues/141).

The two numbers were different populations: 34 questions from 3 of 37
contexts against 589 questions from all 37. Re-run on **matched**
questions, same session, zero errors:

| arm | result |
|---|---|
| `armE-match-retr` — retrieval, `EVIDENCE=3` | 27/34 = 79.4% |
| `armF-match-full` — full prefix, no retrieval | 29/34 = 85.3% |

McNemar over the matched pairs: n=34, **32 agree**, 2 discordant in
full-context's favour, 0 in retrieval's, **p = 0.50**. The 10.3-point gap
becomes 5.9 points on matched questions, and those 5.9 points are **two
questions** — indistinguishable from zero. The ceiling arm reproduced its
recorded 29/34 exactly, so the instrument is sound; the comparison was
not.

**Conclusion: retrieval coverage is not the constraint**, and no claim of
retrieval headroom rests on this pair of numbers. (6 of `armE`'s 7
retrieval misses were `suggest_new_ideas` — the degenerate category
below.)

## Known-degenerate category: `suggest_new_ideas` does not measure memory

93 of the 589 questions are `suggest_new_ideas`. In that category **the
gold answer is essentially always the shortest option**. A classifier that
ignores the question, the memories and the persona entirely and picks the
shortest candidate scores:

| category | pick-shortest accuracy | n |
|---|---|---|
| **suggest_new_ideas** | **98.9%** | 93 |
| recall_user_shared_facts | 15.5% | 129 |
| track_full_preference_evolution | 15.1% | 139 |
| generalizing_to_new_scenarios | 0.0% | 57 |
| provide_preference_aligned_recommendations | 0.0% | 55 |
| recalling_facts_mentioned_by_the_user | 0.0% | 17 |
| recalling_the_reasons_behind_previous_updates | 0.0% | 99 |

Random baseline is 25%. Mean gold length is 245 chars vs 564 for the
distractors (2.3×); every other category sits between 0.75× and 1.17×.
The distractors are the long, persona-woven ones, so retrieved persona
content makes the *wrong* answer look better supported — which is the
entirety of the former "memory net-harms `suggest_new_ideas`" finding.
**That finding is retired: it was an artifact of option length, not a
property of Lunaris retrieval.**

**We deliberately do not fix this.** Teaching the reader a shortness
preference would score ~99% here and mean nothing, while making real
suggestions worse — in production, building on what we know about a user
is the *desirable* behaviour this category punishes. Optimising against a
degenerate category is benchmark-gaming.

Pinned by a regression test so it cannot be re-diagnosed as a retrieval
defect: `crates/lunaris-bench/src/eval/personamem/dataset.rs::suggest_new_ideas_gold_is_the_shortest_option_and_no_other_category_is`
(asserted in both directions — the anomaly must hold there and must NOT
hold anywhere else).

## Per-category (single-reader | floor | oracle cascade)

| category | **sonnet (system)** | floor | *cascade (oracle)* |
|---|---|---|---|
| recalling_the_reasons_behind_previous_updates | **90.9%** | 65.7% | *97.0%* |
| track_full_preference_evolution | **82.0%** | 78.4% | *91.4%* |
| recall_user_shared_facts | **78.3%** | 2.3% | *83.7%* |
| generalizing_to_new_scenarios | **75.4%** | 10.5% | *82.5%* |
| provide_preference_aligned_recommendations | **74.5%** | 21.8% | *80.0%* |
| recalling_facts_mentioned_by_the_user | **58.8%** | 17.6% | *64.7%* |
| ⚠ suggest_new_ideas — **does not measure memory** | 46.2% | 52.7% | *52.7%* |

The `suggest_new_ideas` row is retained for completeness and marked
non-measuring. Do not read its floor-beats-memory shape as a finding, and
do not include it in any headline derived from this table.

## Raw artifacts

Arms are written to `PM_RESULTS_DIR` (default `target/pm`), which is
**gitignored**. The artifacts behind the numbers on this page are already
gone — issue #141 had to answer its questions by re-running six arms
rather than re-tallying, because `target/pm/` had been cleared.

Every future run publishes an envelope before any number from it is
quoted:

```sh
python3 scripts/bench/publish_raw.py --benchmark pm \
  --dir target/pm/32k-memory --expected 37 \
  --operating-point quality --arm memory-sonnet5 \
  --out docs/benchmarks/pm-raw/$(date -u +%F)-32k-memory-sonnet5-quality.json
```

Convention and schema:
[`docs/benchmarks/pm-raw/README.md`](../../../docs/benchmarks/pm-raw/README.md).

## Reproduce

```sh
# arm 1 — memory (headline single-reader number), quality operating point
ARM=memory TOPK=30 NEIGHBORS=2 EVIDENCE=3 DIR=target/pm/32k-memory-v6 \
  scripts/bench/pm/run_pm.sh
# arm 2 — floor
ARM=nomem DIR=target/pm/32k-nomem-v6 scripts/bench/pm/run_pm.sh
# second-reader pass over arm-1 failures (qids file = wrong answers of arm 1)
# NOTE: this arm produces the ORACLE BOUND, not a system result.
ARM=memory READER_MODEL=claude-opus-5 TOPK=30 NEIGHBORS=2 EVIDENCE=3 \
  QIDS_FILE=<fails.txt> OFFSETS_FILE=<ctxs.tsv> \
  DIR=target/pm/32k-memory-v6-opus-fails scripts/bench/pm/run_pm.sh
# combine
python3 scripts/bench/pm/combine.py \
  --primary target/pm/32k-memory-v6 \
  --secondary target/pm/32k-memory-v6-opus-fails --expected 589
```

Arms that will be compared must be run in the same session and published
in the same commit — the control for an arm is the other arm of the same
run, never a number from another day
([`docs/benchmarks/measurement-noise.md`](../../../docs/benchmarks/measurement-noise.md)).
