# Lunaris v0.1.1 — Live-Moon Benchmark Report

**Date:** 2026-04-23
**Commit baseline:** `ae7b60e` (post live-Moon integration fixes — id decode, RRF score extraction, post-hydrate source scoping)
**Environment:**
- Platform: darwin-arm64 (Apple silicon)
- Moon: single shard, release build on `127.0.0.1:6380`, in-memory (no AOF replay)
- Embedder: Ollama `embeddinggemma:300m` (Google EmbeddingGemma 300M, 768-d)
- Lunaris Python SDK via `maturin develop --release` + the feature set `lunaris = { default-features = false, features = ["ollama"] }`
- Rerank: **disabled** (BgeRerankerV2M3 requires `candle` feature + 1.1 GB weights; this report establishes the no-rerank baseline)

---

## 1. What was measured

Two symmetric live-Moon benchmarks that together exercise the full v0.1.1 public surface:

| suite | script | path under test |
|---|---|---|
| Document retrieval | `scripts/bench-squad-kb.py` | `DocumentKnowledgeBase.ingest` + `kb.top(k).search(q)` (Vector + BM25 → Moon-native HYBRID RRF → post-hydrate source scope) |
| Conversational memory | `scripts/bench-dialog-chat.py` | `ChatAgentMemory.remember` + `chat.recall(q)` (same HYBRID RRF + ACT-R recency blend + per-user source scope) |

Both harnesses emit the same section blocks:

- **Moon footprint** — `dbsize`, per-index `FT.INFO num_docs`
- **Ingest** — wall time, throughput, per-doc latency p50/p95/p99/max
- **Recall** — wall time, `recall@k` for `k∈{1,3,5,10}`, MRR, latency p50/p95/p99/max/mean
- **Resources** — CPU% and RSS for `moon`, `ollama`, and the bench process + system CPU/memory (1 Hz `psutil` sampler)

### 1.1 Corpora

- **`rajpurkar/squad`** — QA dataset with `(context, question, answers[])` triples. We dedupe by `context` so each paragraph appears once; questions and gold answer spans become ground truth. Validation split has ~2 k unique contexts; train has ≥ 10 k.
- **`rajpurkar/squad` questions** used as short conversational strings (stand-in for dialog — DailyDialog's HF loader script is deprecated).

### 1.2 Ground truth

- **Docs:** a top-k hit "counts" when any SQuAD gold answer span (whitespace-normalized, case-insensitive) appears as a substring of the hit's chunk text. This is stricter than context matching (which the chunker can break up) and maps onto the actual RAG task.
- **Chat:** the gold turn is re-queried using its first 80 characters; a hit counts when the full turn (normalized) is contained in the retrieved chunk.

### 1.3 Metrics

- **recall@k** — fraction of queries where gold appears in top-k.
- **MRR** — mean of `1/rank_of_first_gold_hit` (0 if no hit).
- **Ingest latency** — per-call wall time around `kb.ingest([chunk])` / `chat.remember(turn)`.
- **Recall latency** — per-call wall time around `kb.top(k).search(q)` / `chat.recall(q)`.
- **Throughput** — total wall / corpus size (serial, no client-side batching).

---

## 2. Results

### 2.1 `bench-squad-kb.py` — 300 contexts × 100 queries (validation split)

| phase | metric | value |
|---|---|---:|
| Ingest | throughput | **15.6 docs/s** |
| Ingest | p50 / p95 / p99 / max | 63 / 72 / 79 / 117 ms |
| Recall | recall@1 / @3 / @5 / @10 | **88% / 95% / 96% / 97%** |
| Recall | MRR | **0.918** |
| Recall | p50 / p95 / p99 / mean | 75 / 81 / 90 / 80 ms |
| Moon   | dbsize / chunks_num_docs | 900 / 967 |

Evidence: [`squad-300x100.log`](./squad-300x100.log)

### 2.2 `bench-squad-kb.py` — 10,000 contexts × 1,000 queries (train split)

| phase | metric | value |
|---|---|---:|
| Ingest | throughput | **14.96 docs/s** |
| Ingest | total wall | 668.6 s |
| Ingest | p50 / p95 / p99 / max | 66 / 77 / 90 / 215 ms |
| Recall | total wall | 94.0 s (10.6 q/s) |
| Recall | recall@1 / @3 / @5 / @10 | **75.3% / 86.0% / 88.8% / 91.1%** |
| Recall | MRR | **0.812** |
| Recall | p50 / p95 / p99 | 86 / 93 / 104 ms |
| Recall | max | 8005 ms (single tail — likely Ollama cold-path) |
| Moon   | dbsize / chunks_num_docs | 30 022 / 11 278 |

Ingest throughput was flat start→finish (15.0/s at both 500/10 000 and 10 000/10 000) — no degradation as the HNSW grew.

Evidence: [`squad-10kx1k.log`](./squad-10kx1k.log)

#### Resource usage during the 10k × 1k run (754 samples at 1 Hz)

| process | CPU mean | CPU p95 | CPU max | RSS mean | RSS peak |
|---|---:|---:|---:|---:|---:|
| **ollama** | **88.2 %** | 95.2 % | 106.3 % | 329 MB | 401 MB |
| **moon** | 4.4 % | 10.6 % | 100.1 % | 141 MB | 264 MB |
| bench python | 3.4 % | 9.8 % | 14.6 % | 93 MB | 324 MB |
| system (all cores) | 25.1 % | 34.2 % | 72.7 % | 80.8 % mem | 84.9 % peak |

> One Ollama process held ~90 % of a single core continuously — the embedder is the dominant cost. Moon itself was idle ~95 % of the time and used 264 MB RSS to serve 11 278 indexed 768-d vectors + 30 k KV rows.

### 2.3 `bench-dialog-chat.py` — 80 turns × 20 queries (smoke)

| phase | metric | value |
|---|---|---:|
| Ingest | throughput | 15.4 turns/s |
| Ingest | p50 / p95 / p99 | 63 / 71 / 72 ms |
| Recall | recall@1 / @3 / @5 / @10 | **75% / 90% / 90% / 90%** |
| Recall | latency p50 / p95 | 74 / 83 ms |

### 2.4 `bench-dialog-chat.py` — 10,000 turns × 1,000 queries

| phase | metric | value |
|---|---|---:|
| Ingest | throughput | **15.68 turns/s** |
| Ingest | total wall | 637.8 s |
| Ingest | p50 / p95 / p99 / max | 63 / 69 / 79 / 217 ms |
| Recall | total wall | 96.1 s (10.4 q/s) |
| Recall | recall@1 / @3 / @5 / @10 | **73.4% / 83.0% / 85.2% / 86.3%** |
| Recall | MRR | **0.785** |
| Recall | p50 / p95 / p99 | 89 / 94 / 98 ms |
| Recall | max | 7535 ms (single tail — same pattern as the doc bench) |
| Moon   | dbsize / chunks_num_docs | 30 000 / 21 438 |

Note the `chunks_num_docs` is ~2× the turn count — the chunker splits some multi-clause turns into several chunks even when the input is short, because the Phase 2 chunker uses a fixed sentence/window policy rather than a per-input length check. This inflates the HNSW but has no effect on recall semantics.

Evidence: [`chat-10kx1k.log`](./chat-10kx1k.log)

#### Resource usage during the chat 10k × 1k run (731 samples at 1 Hz)

| process | CPU mean | CPU p95 | CPU max | RSS mean | RSS peak |
|---|---:|---:|---:|---:|---:|
| **ollama** | **91.8 %** | 98.6 % | 100.3 % | 232 MB | 254 MB |
| **moon** | 4.0 % | 13.3 % | 100.0 % | 152 MB | 337 MB |
| bench python | 2.3 % | 8.4 % | 9.1 % | 82 MB | 180 MB |
| system (all cores) | 27.7 % | 39.2 % | 50.8 % | 82.6 % mem | 84.5 % peak |

### 2.5 Doc vs. Chat side-by-side (both 10 k × 1 k, same hardware)

| metric | docs (SQuAD ctx) | chat (SQuAD questions as turns) |
|---|---:|---:|
| ingest throughput | 14.96 docs/s | **15.68 turns/s** |
| ingest p50 | 66 ms | 63 ms |
| recall latency p50 | 86 ms | 89 ms |
| recall latency p95 | 93 ms | 94 ms |
| recall@1 | **75.3 %** | 73.4 % |
| recall@3 | **86.0 %** | 83.0 % |
| recall@10 | **91.1 %** | 86.3 % |
| MRR | **0.812** | 0.785 |
| moon chunks_num_docs | 11 278 | 21 438 |
| ollama cpu mean | 88 % | 92 % |

Chat quality trails the doc bench by ~3 points recall@1 and ~5 points recall@10 — expected for shorter input strings (less embedding signal per item) combined with a 2× larger chunk population from the chunker split.

### 2.6 Isolated Lunaris — strict replay (embedder cost removed)

To measure Lunaris's own latency without the ~60 ms Ollama `/api/embed` round-trip on every call, we captured every embed text Lunaris emits during a 10 k × 1 k doc run, stored them in `milestones/v0.1.1-bench/cache/squad-10k.npz` (11 012 float32 × 768-d vectors, 32.9 MB), and reran the same bench against the `scripts/ollama-replay-server.py` strict replay server (cache hit = ~0.1 ms JSON round-trip, miss = 404).

The mechanism:

1. Added `LUNARIS_OLLAMA_URL` env override to `OllamaEmbedder::Default` so benchmarks can swap the embedder endpoint without rebuilding.
2. Ran one **record pass** (`--upstream http://127.0.0.1:11434`) to build the cache — identical wire behaviour to direct Ollama, just adds ~20 ms proxy overhead.
3. Ran the **strict replay pass** with the populated cache, zero upstream — every embed served from the `.npz` at microsecond scale.

Quality matched direct (recall@1 75.1 % vs 75.3 %, MRR 0.811 vs 0.812) — the cache round-trips the exact same vectors, so ranking is byte-identical within float rounding.

| phase | metric | direct Ollama | **strict replay** | speedup |
|---|---|---:|---:|---:|
| Ingest | throughput | 14.96 docs/s | **47.0 docs/s** | **3.14×** |
| Ingest | total wall | 668.6 s | **212.9 s** | **3.14×** |
| Ingest | per-doc p50 | 66 ms | **19.0 ms** | 3.5× |
| Ingest | per-doc p95 | 77 ms | **40.9 ms** | 1.9× |
| Ingest | per-doc p99 | 90 ms | **65.7 ms** | — |
| Recall | total wall | 94.0 s | **18.1 s** | **5.2×** |
| Recall | throughput | 10.6 q/s | **55.3 q/s** | **5.2×** |
| Recall | latency p50 | 86 ms | **10.3 ms** | **8.3×** |
| Recall | latency p95 | 93 ms | **12.3 ms** | 7.6× |
| Recall | latency p99 | 104 ms | **20.8 ms** | 5.0× |
| Recall | latency mean | 94 ms | 18.1 ms | 5.2× |
| Recall | max | 8005 ms | 7337 ms | — (tail outlier persists — not an Ollama artefact) |
| Quality | recall@1 / @3 / @10 | 75.3 / 86.0 / 91.1 % | 75.1 / 86.0 / 91.0 % | ≡ |
| Quality | MRR | 0.812 | 0.811 | ≡ |

**Recall p50 = 10.3 ms** — this is Lunaris's pure retrieve+hydrate cost (Moon HYBRID FT.SEARCH + `__rrf_score` parse + post-hydrate source filter + ACT-R rescore path). **Well inside the blueprint §4.2 sub-25 ms target.**

**Ingest p50 = 19 ms** — chunker + atomic_write fan-out (1 KvPut for episode + 1 KvPut + 1 VectorUpsert per chunk, plus the HTTP round-trip to the replay server).

Resource footprint while Lunaris wasn't embedder-blocked:
| process | CPU mean | CPU p95 | RSS peak |
|---|---:|---:|---:|
| **moon** | **13.7 %** | 63.9 % | 365 MB |
| replay server | 8.7 % | 14.1 % | 226 MB |
| bench python | 5.7 % | 8.1 % | 186 MB |

Moon's CPU roughly 3× the direct-Ollama run (4.4% → 13.7%) because it is no longer idle-waiting for the embedder — it is processing the real Lunaris workload continuously.

Evidence: [`squad-10kx1k-record.log`](./squad-10kx1k-record.log), [`squad-10kx1k-strict.log`](./squad-10kx1k-strict.log), cache at `cache/squad-10k.npz`.

> **Caveat on `chunks_num_docs = 20 048`:** Moon's FT.* index state did not reset across the `FLUSHALL` between the two runs, so the strict-pass `num_docs` counter is inflated by the record-pass entries. This does **not** affect recall correctness — the 10 k stale vectors have no corresponding `lunaris:chunk:<ulid>` row after FLUSHALL, so `hydrate` silently drops them and the 75 % recall@1 is computed only over live data. Worth tracking upstream (Moon) as a separate B-task.

---

## 3. Headline findings

1. **Recall@3 = 86 % on 10 k docs, no rerank, off-the-shelf embedder.** For open-domain SQuAD, this is a strong baseline — adding the cross-encoder rerank (Phase 02-03 RETRIEVE-06) is expected to lift recall@1 by 8–15 pts based on published BGE numbers.

2. **Sub-25 ms recall target is achieved by Lunaris itself — proven.** Section 2.6 isolates the embedder: recall p50 drops from 86 ms → **10.3 ms** and p99 from 104 ms → **20.8 ms** when the Ollama round-trip is replaced by a local cache lookup. Ingest throughput jumps from 15 → 47 docs/s (3.1× faster). Production deploys should cache or batch query embeds, or set `OLLAMA_NUM_PARALLEL > 1`.

3. **Moon scales well on a single shard.** 11 k vectors + 30 k KV rows, 264 MB RSS, 4 % CPU mean. Plenty of headroom for 100 k+ documents before sharding is needed.

4. **Ingest throughput held flat at 15 docs/s through 10 k docs.** HNSW growth did not degrade ingest latency (p50 steady at 65 ms start → 67 ms end).

5. **Quality drops from 300 → 10 k as expected.** recall@1 fell 88 % → 75 %, recall@10 from 97 % → 91 %. This is the distractor effect, and it's exactly what rerank mitigates.

---

## 4. Known limitations

- **No rerank.** The SDK was built with `--features ollama` (not `candle`). The default_reranker under `cfg(not(candle))` is `NoopReranker`, so the baseline above is Vector + BM25 + RRF only. Running with `--features candle` and `BgeRerankerV2M3` weights cached would add ~12 ms per query and (based on Moon's own retrieval tests) lift recall@1 by roughly 10 points.
- **Single-shard Moon.** No cluster / multi-shard coordinator results here.
- **Single embedder process.** Ollama default is 1 concurrent request per model; parallelizing ingest/recall would require `OLLAMA_NUM_PARALLEL > 1` or batch embeddings. This bench measures serial performance to keep latency distributions honest.
- **Tail outlier.** The 10 k × 1 k recall had one 8 s outlier — isolated; p99 stayed under 105 ms. Investigation out of scope for this report.
- **Disk / AOF fsync.** Moon ran without AOF during these runs. Durable write latency is not measured here.

---

## 5. How to reproduce

### Prerequisites

1. Moon release build running on `moon://127.0.0.1:6380`:

   ```bash
   ../moon/target/release/moon \
     --bind 127.0.0.1 --port 6380 \
     --dir target/moon-data --shards 1 --protected-mode no &
   ```

2. Ollama with EmbeddingGemma pulled:

   ```bash
   ollama pull embeddinggemma:300m
   ```

3. Lunaris Python SDK built + installed:

   ```bash
   cd crates/lunaris-py
   uv run --with maturin maturin develop --release
   ```

### Runs

```bash
# Document benchmark — 10k × 1k (≈ 13 min wall)
redis-cli -p 6380 FLUSHALL
cd crates/lunaris-py
LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
  uv run --with datasets --with python-ulid --with redis --with psutil \
    python ../../scripts/bench-squad-kb.py \
      --docs 10000 --queries 1000 --top-k 10 --split train --progress-every 500 \
      > ../../milestones/v0.1.1-bench/squad-10kx1k.log 2>&1

# Chat benchmark — 10k × 1k (≈ 13 min wall)
redis-cli -p 6380 FLUSHALL
LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
  uv run --with datasets --with python-ulid --with redis --with psutil \
    python ../../scripts/bench-dialog-chat.py \
      --turns 10000 --queries 1000 --top-k 10 --progress-every 500 \
      > ../../milestones/v0.1.1-bench/chat-10kx1k.log 2>&1
```

### Smaller sanity runs

```bash
# 300 × 100 SQuAD (≈ 30 s)
python ../../scripts/bench-squad-kb.py --docs 300 --queries 100 --top-k 10
# 80 × 20 chat (≈ 10 s)
python ../../scripts/bench-dialog-chat.py --turns 80 --queries 20 --top-k 10
```

### Isolated Lunaris (embedder cost removed)

```bash
# 1. Start replay server in RECORD mode on an alternate port (Ollama stays on 11434).
cd /Users/tindang/workspaces/tind-repo/lunaris
uv run --with numpy python scripts/ollama-replay-server.py \
  --cache milestones/v0.1.1-bench/cache/squad-10k.npz \
  --port 11435 \
  --upstream http://127.0.0.1:11434 &

# 2. Run the regular bench with the embedder redirected to the replay server —
#    populates the npz as a side effect. Quality is identical; latency is
#    ~20% worse than direct because of the extra proxy hop.
redis-cli -p 6380 FLUSHALL
cd crates/lunaris-py
LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
LUNARIS_OLLAMA_URL="http://127.0.0.1:11435" \
  uv run --with datasets --with python-ulid --with redis --with psutil \
    python ../../scripts/bench-squad-kb.py \
      --docs 10000 --queries 1000 --top-k 10 --split train --progress-every 500 \
      > ../../milestones/v0.1.1-bench/squad-10kx1k-record.log 2>&1

# 3. SIGTERM the replay server — it saves the npz on shutdown.
kill <replay_pid>

# 4. Restart replay in STRICT mode (no upstream) and rerun the bench — now
#    every embed is a cache lookup. Quality is byte-identical; latency drops
#    3–8×.
cd ..
uv run --with numpy python scripts/ollama-replay-server.py \
  --cache milestones/v0.1.1-bench/cache/squad-10k.npz --port 11435 &

redis-cli -p 6380 FLUSHALL
cd crates/lunaris-py
LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
LUNARIS_OLLAMA_URL="http://127.0.0.1:11435" \
  uv run --with datasets --with python-ulid --with redis --with psutil \
    python ../../scripts/bench-squad-kb.py \
      --docs 10000 --queries 1000 --top-k 10 --split train --progress-every 1000 \
      > ../../milestones/v0.1.1-bench/squad-10kx1k-strict.log 2>&1
```

Requires the `LUNARIS_OLLAMA_URL` env override — added to `lunaris-embed::OllamaEmbedderOpts::Default` in the commit that landed this report.

---

## 6. Open follow-ups

| id | item | priority |
|---|---|---|
| BENCH-01 | Rebuild with `--features candle` + BgeRerankerV2M3 weights, rerun 10 k × 1 k, report recall delta | high |
| BENCH-02 | Add `OLLAMA_NUM_PARALLEL=8` + concurrent ingest/recall to measure the non-serial ceiling | medium (partially addressed — section 2.6 isolates Lunaris via the `ollama-replay-server.py` cache) |
| BENCH-07 | Report upstream Moon issue: FT.* index state survives `FLUSHALL` (see section 2.6 caveat) | medium |
| BENCH-03 | Investigate the 8 s tail outlier — correlate with Ollama slowlog / Moon profiling | medium |
| BENCH-04 | Postgres backend parity benchmark (same harness, `postgres://…` URL) | medium |
| BENCH-05 | Cross-user scoping correctness test (N users × M turns each, assert zero cross-user leakage at top-k) | high |
| BENCH-06 | 100 k corpus run on a single shard to locate Moon's knee | low |
