# PersonaMem results — 32k split (2026-08-17)

Reference point: [TencentDB-Agent-Memory](https://github.com/TencentCloud/TencentDB-Agent-Memory)
publishes **PersonaMem 76% with memory / 48% without** (split and reader
unstated). Every number below names its split and reader, per the harness's
own discipline.

## Headline

| configuration | accuracy |
|---|---|
| **Two-reader combined (oracle ensemble, see honesty note)** | **81.8% (482/589)** |
| Lunaris memory, claude-sonnet-5 reader | 75.0% (442/589) |
| No-memory floor, claude-sonnet-5 reader | 41.9% (247/589) |
| Reader ceiling (full prefix, no retrieval, contexts {1,15,18}) | 85.3% (29/34) |

- **Memory delta: +33.1 points** (75.0 vs 41.9) — larger than Tencent's
  published +28 (76 vs 48), on a lower floor.
- Zero transport errors and zero temporal-leak trips in every run; ERR is
  never scored wrong (H3).

## Measured configuration

Production path: `CodingSessionMemory::write` → `Lunaris::ingest`, recall via
`recall_with_degraded_check()` hybrid root (Vector ∧ BM25 → RRF →
cross-encoder rerank → top-k), graph OFF. Knobs: `TOPK=30 POOL=30
NEIGHBORS=2 EVIDENCE=3`, embedder granite-embed-r2 (Ollama), reranker
bge-reranker-v2-m3, readers via a local Claude-CLI bridge. Incremental
append-only ingest per shared context; a question sees exactly
`messages[..end_index]` (structural + runtime temporal-honesty guards).

## Combined-number honesty note

The 81.8% is a **two-reader oracle ensemble**: claude-sonnet-5 answered all
589 questions; claude-opus-5 then re-answered ONLY the 147 questions Sonnet
got wrong (`LUNARIS_EVAL_PM_QIDS_FILE`), fixing 40 of them (27.2%). Because
gold labels routed which questions reached the second reader, this is an
upper bound on a two-reader cascade, not a single-reader measurement — a
deployable cascade needs a gold-free routing rule (e.g. self-reported
confidence). The clean single-reader number is 75.0%.

## Per-category (combined | sonnet-only | floor)

| category | combined | sonnet | floor |
|---|---|---|---|
| recalling_the_reasons_behind_previous_updates | 97.0% | 90.9% | 65.7% |
| track_full_preference_evolution | 91.4% | 82.0% | 78.4% |
| recall_user_shared_facts | 83.7% | 78.3% | 2.3% |
| generalizing_to_new_scenarios | 82.5% | 75.4% | 10.5% |
| provide_preference_aligned_recommendations | 80.0% | 74.5% | 21.8% |
| recalling_facts_mentioned_by_the_user | 64.7% | 58.8% | 17.6% |
| suggest_new_ideas | 52.7% | 46.2% | 52.7% |

`suggest_new_ideas` remains the outlier: the floor matches the best memory
number — PersonaMem's fresh-idea distractors are built FROM the persona
history (gold is never a first-person anecdote, 93/93; it typically extends a
known activity), so retrieved persona content still net-misleads readers
there. See the two-sided guidance rationale in
`crates/lunaris-bench/src/eval/personamem/reader.rs`.

## Reproduce

```sh
# arm 1 — memory (headline single-reader number)
ARM=memory TOPK=30 NEIGHBORS=2 EVIDENCE=3 DIR=target/pm/32k-memory-v6 \
  scripts/bench/pm/run_pm.sh
# arm 2 — floor
ARM=nomem DIR=target/pm/32k-nomem-v6 scripts/bench/pm/run_pm.sh
# second-reader pass over arm-1 failures (qids file = wrong answers of arm 1)
ARM=memory READER_MODEL=claude-opus-5 TOPK=30 NEIGHBORS=2 EVIDENCE=3 \
  QIDS_FILE=<fails.txt> OFFSETS_FILE=<ctxs.tsv> \
  DIR=target/pm/32k-memory-v6-opus-fails scripts/bench/pm/run_pm.sh
# combine
python3 scripts/bench/pm/combine.py \
  --primary target/pm/32k-memory-v6 \
  --secondary target/pm/32k-memory-v6-opus-fails --expected 589
```
