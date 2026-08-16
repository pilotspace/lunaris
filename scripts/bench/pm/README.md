# PersonaMem bench harness (`scripts/bench/pm/`)

Measures Lunaris on **PersonaMem** (HF `bowen-upenn/PersonaMem`, MIT) over the
PRODUCTION ingest/recall path, so the number is comparable to what other agent
memory systems publish.

> **Reference point.** TencentDB-Agent-Memory reports **PersonaMem 76% with
> memory / 48% without**. Any Lunaris claim must state the **split** and the
> **reader model** — the reader answers the multiple-choice question from what
> recall surfaced, so it is half of the measurement.

## What the harness does

1. Groups the split's questions by `shared_context_id` and sorts each group by
   `end_index_in_shared_context`.
2. Per shared context: `FLUSHALL` + `FT.DROPINDEX` the bench Moon, open a fresh
   `Lunaris`, then walk the persona's interaction history **forward, once**,
   writing one document per message through `CodingSessionMemory::write` →
   `Lunaris::ingest`.
3. A question is answered the moment its prefix — and nothing after it — is in
   the store. The store is append-only, so recall physically cannot see later
   context; a hit from beyond the prefix marks the question `ERR` and is
   logged, never silently scored.
4. Recall runs the hybrid production root (Vector ∧ BM25 → RRF → cross-encoder
   rerank → top-k), the same configuration the LongMemEval harness measures on.
   The graph pipeline stays OFF (default).
5. The reader gets the retrieved messages (chronological) + the user message +
   the lettered options, and must reply with one letter. Scoring is an **exact
   letter match** — no LLM judge, so none of LongMemEval's ±5-point judge
   noise floor.

## Prerequisites

```bash
# 1. Bench Moon on 6399 — NEVER 6379 / 6380 / 6381 (lib.sh hard-refuses those).
LME_MOON_BIN=/path/to/moon-0.8.5+ scripts/bench/lme/moon_watchdog.sh &

# 2. Warm embedder (Ollama, granite-embed-r2) on 11434, and the reader chat
#    bridge (Ollama-shaped) on 11435. Neither is started by these scripts.

# 3. The eval binary.
cargo build --release -p lunaris-bench --bin lunaris-evals \
  --features embed-remote,llamacpp,metal
```

The dataset downloads itself on first use into `LUNARIS_EVAL_CACHE_DIR`
(default `~/.cache/lunaris/eval-hub`). It is never written into the repo.

## Running

```bash
scripts/bench/pm/run_pm.sh --dry-run        # preflight: paths, ports, config

# 2-question smoke against the first context
OFFSETS_FILE=scripts/bench/pm/contexts/offsets_smoke.tsv QLIMIT=2 \
  scripts/bench/pm/run_pm.sh

# full 32k arm (37 contexts, 589 questions)
scripts/bench/pm/run_pm.sh

# full 128k arm (60 contexts, 2727 questions)
SPLIT=128k OFFSETS_FILE=scripts/bench/pm/contexts/offsets_128k.tsv \
  scripts/bench/pm/run_pm.sh

# no-memory floor arm (Tencent's 48% column) — same reader, no retrieval
ARM=nomem scripts/bench/pm/run_pm.sh
```

Runs are resumable: a context is skipped only when its artifact says
`PASS`/`FAIL` **and** its log ends with `PM_RUN_DONE … "errors":0`. A config or
binary change aborts the resume instead of mixing runs (`config.fp`).

## Scoring

```bash
scripts/bench/pm/tally.py --dir target/pm/32k-memory --expected 37
scripts/bench/pm/tally.py --dir target/pm/32k-memory --expected 37 --json
```

`RUN NOT FINAL` prints until every context has an artifact and `ERR == 0`. A
partial arm is not comparable to a complete one — do not publish it.

## Split shapes (verified 2026-08-16)

| split | contexts | questions | questions / context |
|---|---|---|---|
| 32k  | 37 | 589  | 5–28   |
| 128k | 60 | 2727 | 24–64  |
| 1M   | 31 | 2674 | 61–114 |

`contexts/offsets_*.tsv` are just `0..n-1`; regenerate after a dataset bump
with the distinct-`shared_context_id` count of the split's questions CSV.

## Environment

| variable | default | meaning |
|---|---|---|
| `MOON_PORT` | `6399` | bench Moon; 6379/6380/6381 are hard-refused |
| `SPLIT` | `32k` | `32k` \| `128k` \| `1M` |
| `ARM` | `memory` | `memory` \| `nomem` (no-retrieval floor) |
| `READER_MODEL` | `claude-sonnet-5` | reader; MiniMax names require `MINIMAX_API_KEY` |
| `CHAT_URL` | `http://127.0.0.1:11435` | Ollama-shaped reader bridge |
| `OLLAMA_URL` | `http://127.0.0.1:11434` | embedder |
| `TOPK` / `POOL` | `10` / `30` | recall shape |
| `QOFFSET` / `QLIMIT` | `0` / all | question slice within a context |
| `QTIMEOUT` | `3600` | per-context watchdog, seconds |

The harness reads `LUNARIS_EVAL_PM_*` directly; `run_pm.sh` is the mapping from
these operator-facing names onto them, and every one of them lands in the H4
config fingerprint.
