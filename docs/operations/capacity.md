# Capacity & latency envelope — GA-2b

This document states Lunaris's **target corpus**, the **measured recall
latency envelope** at that corpus on the GA-1 unified production root
(`lunaris_retrieve::production_root`), and the **measured cost of the
opt-in rerank stage** (`LUNARIS_RECALL_RERANK`). It anchors the rows in
[`slo.md`](slo.md) that were provisional pending the GA-2 capacity study.

**TL;DR:**

- **Target corpus: 100,000 documents per scope, single-shard Moon.** At
  that corpus the default recall config measures **p50 19–22 ms /
  p99 23.4–24.4 ms** (engine-side, retrieval-only) — the **25 ms p50
  contract holds, with ≤ 25 % headroom and a p99 grazing the line.**
- **The opt-in rerank stage costs ~1.3 s per recall** at its default
  depth (`top_in = 2k = 60`) on this class of hardware — it is a
  quality stage, not a latency-class stage, and any deployment enabling
  it must re-derive its latency SLO (see §4).
- **Graph-ON recall measures ~39 ms p50** at the same corpus — the
  opt-in fact/entity legs roughly double the default config's cost.
- Do **not** quote the product line "sub-25 ms over millions of facts"
  as measured: the measured envelope is 100k docs, and the 1k→100k
  scaling trend (0.7 ms → ~20 ms p50) says millions will NOT meet 25 ms
  p50 on this hardware without further storage-side work (§5).

## 1. Environment (quote this with every number)

| | |
|---|---|
| Machine | Apple M4 Pro, 24 GB RAM, macOS 15.7.9 (`laptop-arm64` class) |
| Background load | dev box, not a clean lab: a game client idled at ~1 core during runs; observed run-to-run p50 drift ± 3 ms (see the two baseline rows in §3) |
| Moon | **v0.8.5**, release build, single shard, scratch instance on port 6401, fresh `--dir`, flags `--shards 1 --protected-mode no --disk-free-min-pct 1 --max-unflushed-immutable-segments 4096` |
| Lunaris | workspace @ `b17f9e6` + the GA-2b harness commit, `cargo build --release -p lunaris-bench --features llamacpp,metal --bin recall-latency` |
| Reranker | `bge-reranker-v2-m3.Q5_K_M.gguf` (`~/.lunaris/models/`), lazy-loaded, full Metal offload (`n_gpu_layers = MAX`; CPU-forced control run was 6.6× slower — Metal is confirmed engaged) |
| Harness | `crates/lunaris-bench/src/bin/recall_latency.rs` via `scripts/bench/perf/recall_latency.sh` |

## 2. Corpus + methodology

**Corpus (the stated target):** 100,000 synthetic documents of ~300 bytes
each (deterministic prose, seed `0x6A2B_2026_0818_0001`, vocabulary +
topic markers shared with the query generator). Ingested through the
**real public ingest paths** — `ScopedLunaris::ingest` for 80 % of docs,
`ScopedLunaris::ingest_structured` with 1 entity + 1 fact for every 5th
doc (→ 20,000 facts, 500 distinct entities), one `atomic_write` per doc
(INGEST-04). Build cost: **~85 s at ~1.2 k docs/s** (8 concurrent
ingests), 550,800 Moon keys, 1.4 GB on disk.

**Methodology — retrieval-only (the contract decomposition).** Query
embedding runs through `StubEmbedder` (768-d, microseconds), exactly the
decomposition the v0.2.x strict-replay baseline and the
[v0.7 rerun](../benchmarks/v0.7-moon-v030-rerun.md) established for the
25 ms contract: **embed out of the loop, engine (Moon FT.SEARCH + RRF
fuse + hydrate [+ rerank]) in the loop.** Corpus vectors come from the
same embedder, so the vector leg does real 768-d KNN work. The rerank
config is the one config with real model inference in the timed path —
that stage's cost is what GA-2b prices. Queries are sequential (one in
flight), k = 30 (the production root's default), **500 timed queries
after 50 warmup** per config (rerank: two 250-query processes after 10
warmup each, disjoint query offsets, raw samples merged — a single run
would exceed the operator's per-command timeout).

## 3. Measured envelope @ 100k docs (ms, engine-side)

| Config | mean | p50 | p95 | p99 | max | first call |
|---|---|---|---|---|---|---|
| **(a) production default** (graph OFF, rerank OFF) — run 1 | 19.5 | **19.2** | 22.3 | **23.4** | 24.0 | 42.5 |
| (a) production default — run 2 (same corpus, later, disjoint queries) | 22.2 | **22.4** | 24.1 | **24.4** | 24.6 | — |
| **(b) rerank ON** (`LUNARIS_RECALL_RERANK=1`, default `top_in=60`), n=500 merged | 1309.5 | **1301.3** | 1367.0 | **1510.7** | 2585.0 | 2363–2672 † |
| (b′) rerank ON, `LUNARIS_RECALL_RERANK_TOP_IN=30`, n=100 | 580.9 | 575.6 | 598.9 | 996.7 | — | 1598.6 |
| **(c) graph ON** (chunks ∧ facts legs, Navigate + BM25) | 39.0 | **39.1** | 40.6 | **41.2** | 41.8 | 43.6 |

† First call includes the one-time lazy bge GGUF load: **~1.0–1.4 s**
(first_call − steady p50 across the three rerank processes). Steady
state is reached immediately after; the split runs agree to within 4 ms
on p50.

### The 25 ms p50 contract at the target corpus

- **Default config: HOLDS.** p50 19.2–22.4 ms across runs — but the
  headroom is now **≤ 25 %**, and p99 (23.4–24.4 ms) sits within 1 ms
  of the 25 ms line. For scale intuition only: the 2026-04-23 10k-doc run
  measured p50 10.3 / p99 20.8 ms, so 10× corpus ≈ 2× p50 — but that
  figure was **retracted as a published claim on 2026-08-21** (Ollama +
  EmbeddingGemma 300M, k=3, a stack deleted in v0.4/v0.6; its
  [methodology page](../benchmarks/v0.2.x/README.md) never held a
  captured number). Do not quote it; use it only as the shape of the
  curve. The tail stays flat (p99 ≈ 1.1–1.2× p50 — the v0.6
  hydration-fanout fix is still doing its job).
- **Graph-ON: does NOT hold** (p50 ~39 ms). The graph is opt-in and the
  contract is written against the default config, but deployments that
  flip `LUNARIS_GRAPH_ENABLED` at this corpus size should budget ~2×
  the default latency.
- **Rerank-ON: not a 25 ms-class stage** — see §4.

## 4. The opt-in rerank stage's cost (GA-1's `LUNARIS_RECALL_RERANK`)

At the default pool (`top_in = 2k = 60` fused candidates) the
cross-encoder pass adds **~1.28 s p50** on top of the fused retrieval —
**~21 ms per candidate pair** on full-Metal-offload M4 Pro (Q5_K_M
bge-reranker-v2-m3, per-pair forward passes, no batching). Cost is
linear in the pool: `top_in=30` halves it (~576 ms p50). The CPU-forced
control (`LUNARIS_DEVICE=cpu`) measured ~7.4 s p50 — 6.6× slower, so
the Metal number is the floor on this box, not a misconfiguration.

Consequences:

- Blueprint §4.2 allocated **12 ms p50 / 35 ms p99** to the rerank
  pass. The measured stage is **~100× that allocation.** Do not treat
  the blueprint row as achievable with per-pair GGUF cross-encoding;
  closing the gap needs batched rerank inference (the bucketed-batching
  work that landed for the embedder), a smaller/distilled cross-encoder,
  or a hard `top_in` cap.
- The 100 ms recall-latency SLO in [`slo.md`](slo.md) §2 **cannot be met
  with rerank ON** at any measured `top_in`. A deployment that enables
  `LUNARIS_RECALL_RERANK` must re-derive its latency SLO (seconds-class)
  and re-provision `deploy/prometheus/lunaris-alerts.yml` thresholds
  before rollout — otherwise the burn-rate alerts page immediately.
- The **one-time lazy model load (~1.0–1.4 s)** lands on the first
  reranked recall of the process. Operators who care about first-request
  latency should issue one throwaway reranked recall at boot.

## 5. Scaling notes

- **Single shard is a hard constraint** —
  [RFC 0008](../rfcs/0008-sharded-moon-ingest.md): Lunaris ingest
  (`TXN.*`) and `FT.NAVIGATE` on the recall path are both
  single-shard-only on sharded Moon. The envelope above is therefore
  the envelope of **one Moon shard**; there is no horizontal read-path
  escape hatch today. Scale-out currently means **scope-per-instance**
  partitioning, not shards.
- **Corpus scaling trend (same box, same config):** 1k docs → 0.7 ms
  p50; 100k docs → 19–22 ms p50. The growth is super-logarithmic in the
  measured range; extrapolating to "millions of bi-temporal facts"
  clearly exceeds 25 ms p50 on this hardware. Until a measured run at
  1M+ exists (with whatever storage-side work it takes — e.g. Moon SQ8
  quantization landed in v0.6.0 with recall 1.000 on its own bench),
  **the defensible public claim is the 100k envelope in §3, not
  "millions".**
- **Ingest at build rate:** ~1.2 k docs/s sustained. Two Moon-side
  pressure behaviors surfaced and are handled by the harness (retry
  with backoff): `busy: compaction backlog` when running with default
  `--max-unflushed-immutable-segments` (fix: the production launchd
  flag value 4096, now also in the runner), and one transient
  `AOF fsync failed; write not durable` refusal during an AOF-rewrite
  overflow drain at ~93k docs (write rolled back cleanly; retry
  succeeded; key-count audit confirmed no duplication).

## 6. Reproduction

```bash
# One command (stands up its own scratch Moon on $GA2B_PORT, default 6399;
# 6379/6380/6381 are hard-refused; ingests 100k; measures all three configs):
cargo build --release -p lunaris-bench --features llamacpp,metal --bin recall-latency
GA2B_PORT=6401 GA2B_MOON_BIN=/path/to/moon/target/release/moon \
  scripts/bench/perf/recall_latency.sh all

# Phased (what this study actually ran, chunked for bounded shell calls):
scripts/bench/perf/recall_latency.sh up
scripts/bench/perf/recall_latency.sh ingest 0 50000
scripts/bench/perf/recall_latency.sh ingest 50000 100000
scripts/bench/perf/recall_latency.sh measure baseline
GA2B_QUERIES=250 GA2B_WARMUP=10 GA2B_QUERY_OFFSET=0   scripts/bench/perf/recall_latency.sh measure rerank a
GA2B_QUERIES=250 GA2B_WARMUP=10 GA2B_QUERY_OFFSET=300 scripts/bench/perf/recall_latency.sh measure rerank b
scripts/bench/perf/recall_latency.sh measure graph
scripts/bench/perf/recall_latency.sh down
# Results + raw per-query samples land in target/ga2b/.
```

The result envelopes behind §3 are committed verbatim at
[`docs/benchmarks/ga2b-raw/`](../benchmarks/ga2b-raw/README.md).

This is the **local gate command** for the envelope; it is deliberately
not wired into CI (a ~10-minute live-Moon + Metal job — CI integration
is a separate decision).
