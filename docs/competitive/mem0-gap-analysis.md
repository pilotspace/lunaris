# Mem0 Competitive Gap Analysis — Production-Hardening Backlog

> Task `mem0-gap-analysis` (milestone `mem0-parity-hardening`). Contract §3 v1, frozen 2026-06-14.
> Evidence-audit-and-rank: every Lunaris claim cites a `path:symbol`; every Mem0 claim cites a dated URL.
> **Built ≠ wired** — a primitive existing in the tree is not parity; the production call site is the proof.
> Gate: `python3 .add/tasks/mem0-gap-analysis/tests/validate_gap_analysis.py docs/competitive/mem0-gap-analysis.md`

## A. Executive summary

Lunaris is **ahead of Mem0 on its core differentiators** (provable atomicity, DB-enforced tenant
isolation, integrated graph with no LLM on the read path) but carries **two production-hardening
liabilities that the marketing does not admit**: a circuit breaker and fallback extractor that are
*built but not wired* onto the hot path, and an accuracy eval gauntlet whose LoCoMo/LongMemEval/ER-F1
harnesses are *stubs that always score 0.0*. The first is the only **P0** (a stalled Moon call hangs
ingest with no timeout). The Mem0-parity feature gaps (write-time contradiction resolution, typed
multi-level memory, framework adapters) are real but **P1** — competitive, not data-risk — per the
frozen rubric (hardening-first).

| # | dimension | verdict | headline |
|---|-----------|---------|----------|
| 1 | reliability / IO-failsafe | partial(built-not-wired) | CircuitBreaker + FallbackExtractor exist but only test code builds them; Moon IO has no per-op timeout → ingest can hang |
| 2 | eval / accuracy | partial(built-not-wired) | LoCoMo/LongMemEval/ER-F1 harnesses return hardcoded 0.0; no verified accuracy number exists vs Mem0's 92.5 LoCoMo |
| 3 | observability / ops | partial(built-not-wired) | metrics+tracing+graceful-shutdown wired, but `/healthz` is a static stub that passes with a dead Moon |
| 4 | correctness / security | ahead | one `atomic_write`, token-claim-bound `Scope` newtype enforced into the per-scope Moon keyspace / FT index / graph name; Mem0 has app-layer-only isolation + a graph-delete data-retention bug. **(The Postgres RLS half of this row was true through 0.6.2 and is now moot — the backend was deleted in 0.7.0.)** |
| 5 | memory-update-intelligence | gap-missing | Lunaris ingest is append-only; no write-time ADD/UPDATE/DELETE/NOOP or contradiction resolution (CONTESTED vs Mem0 version — see §B) |
| 6 | multi-level-memory + categories | partial(built-not-wired) | metadata filtering is wired at recall, but no typed user/session/agent levels and no auto-categorization |
| 7 | graph-quality | ahead | graph extraction + traversal wired into RRF recall with no LLM on the read path; Mem0 OSS v3 removed graph (Platform-only, with open deletion bugs) |
| 8 | SDK / DX / integrations | partial(built-not-wired) | Python/TS SDKs + MCP (17 tools) ship, but LangGraph/CrewAI/Letta adapters are promised in docs yet absent in code |

## B. Methodology

- **Mem0 sources** are live pages accessed **2026-06-14** (the assistant's training knowledge of Mem0
  is Jan-2026 and was not trusted). Mem0's accuracy figures are **self-reported** (ECAI-2025 paper +
  April-2026 blog/research pages); no independent third-party replication was found.
- **Lunaris evidence** is by code reading via serena: every claim resolves to a `path:symbol`, and for
  "ahead" rows the production call site (not a definition) was confirmed.
- **Eval gauntlet could NOT be run here** and would not yield a real number even if run: the accuracy
  harnesses are stubbed (§D), and the CI gauntlet soft-skips without a live Moon + cached model weights.
  Per the frozen §1 fallback rule, §D uses the *historical, manually-collected* latency numbers and flags
  accuracy as **needs live rerun + un-stub**.
- **CONTESTED finding (memory-update-intelligence):** the Mem0-research pass reports that Mem0 **v3
  (Apr 2026)** replaced the two-pass ADD/UPDATE/DELETE/NOOP "memory manager" with a single-pass ADD-only
  algorithm (contradiction handled at retrieval time); the OSS `main` branch read shows the 4-op manager
  still present. The verdict is ranked on the **Lunaris-side fact** (no contradiction resolution at all),
  which holds regardless of which Mem0 version is canonical. Both Mem0 sources are cited in the row.

## C. Gap table

| dimension | mem0_capability | lunaris_reality | evidence_anchor | mem0_source | verdict | severity |
|-----------|-----------------|-----------------|-----------------|-------------|---------|----------|
| reliability | timeouts via httpx; retry caller-delegated; no breaker | breaker+fallback exist but test-only; Moon IO no per-op timeout | crates/lunaris-storage-moon/src/atomic.rs:run_ops | https://github.com/mem0ai/mem0/blob/main/mem0/client/main.py (2026-06-14) | partial(built-not-wired) | P0 |
| eval | self-reported LoCoMo 92.5 / LongMemEval 94.4; OSS bench framework | LoCoMo/LongMemEval/ER-F1 harnesses hardcode 0.0; only latency e2e is real | crates/lunaris-bench/src/eval/locomo.rs | https://mem0.ai/research-3 (2026-06-14) | partial(built-not-wired) | P1 |
| observability | AgentOps/Keywords AI plug-ins; basic /requests log; no /metrics | 10 Prometheus metrics + tracing + graceful shutdown wired; /healthz a static stub | crates/lunaris-server/src/routes/healthz.rs | https://docs.mem0.ai/integrations (2026-06-14) | partial(built-not-wired) | P1 |
| correctness-security | app-layer user_id scoping; no RLS; graph delete leaves orphans (#3245) | one atomic_write; token-claim-bound Scope; per-scope Moon keyspace + FT index + graph (Postgres RLS gone with the backend in 0.7.0) | crates/lunaris-ingest/src/pipeline.rs:assemble_and_write | https://github.com/mem0ai/mem0/issues/3245 (2026-06-14) | ahead | P2 |
| memory-update-intelligence | per-observation LLM ADD/UPDATE/DELETE/NOOP reconciliation (v2); v3 single-pass ADD | append-only ingest; ACT-R archive is recency-decay, not contradiction; no UPDATE/DELETE | crates/lunaris-consolidate/src/act_r.rs:ActRConsolidator | https://github.com/mem0ai/mem0/blob/main/mem0/memory/main.py (2026-06-14) | gap-missing | P1 |
| multi-level-memory | user/agent/app/run scope axes + category metadata filtering | Scope is one opaque string; recall metadata filter wired; no typed levels/categories | crates/lunaris-core/src/scope.rs:Scope | https://docs.mem0.ai/platform/features/entity-scoped-memory (2026-06-14) | partial(built-not-wired) | P1 |
| graph-quality | Mem0g Neo4j, LLM on read path; OSS v3 removed graph; open delete bug | graph extract+traverse wired into RRF recall; no LLM on read; MERGE dedup | crates/lunaris-retrieve/src/operators/navigate.rs:Navigate | https://docs.mem0.ai/migration/oss-v2-to-v3 (2026-06-14) | ahead | P2 |
| sdk-dx | Python/TS/Go SDKs + 13 framework integrations + hosted API | Python/TS SDKs + MCP (17 tools); LangGraph/CrewAI/Letta adapters promised but absent | crates/lunaris-py/src/lib.rs | https://docs.mem0.ai/integrations (2026-06-14) | partial(built-not-wired) | P1 |

### Per-dimension detail (evidence)

**1 · reliability** — `crates/lunaris-core/src/circuit_breaker.rs::CircuitBreaker` (RFC 0007) is fully
implemented and wired into `crates/lunaris-extract/src/fallback.rs::FallbackExtractor`, but
`FallbackExtractor::new` is constructed **only in test code** — the production `open()` path installs a
plain extractor via `crates/lunaris/src/handle.rs:352 default_extractor()` with no breaker. Moon IO
(`atomic.rs`, `vector.rs`, `keyword.rs`, `graph.rs`) issues HSET/FT.SEARCH/TXN with **no
`tokio::time::timeout`** — only a 300 s connect-time guard (`client.rs:203`). A stalled Moon HSET during
`atomic_write` blocks the ingest handler indefinitely. (Postgres keyword has a 500 ms timeout; LLM calls
have `GenOpts::timeout` — Moon is the hole.)

**2 · eval** — `longmemeval.rs:compute_j_score` returns `Ok(0.0)`; `locomo.rs` hardcodes `j_score = 0.0`;
`er_f1.rs` hardcodes `f1 = 0.0` (all commented "deferred to 05-HUMAN-UAT"). `eval-gauntlet.yml:124`
*does* gate on a `FAIL` row, but the harnesses soft-skip without `MOON_URL`/weights, and the
`moondb/moon` CI image is flagged uncertain. Only the e2e latency budget (EVAL-05/06) exercises the real
engine. Net: the accuracy thresholds (J≥65/70, F1≥0.80) have **never run against shipped code**.

**3 · observability** — metrics (`metrics.rs`, 10 series) are timed on the hot path
(`routes/ingest.rs:52`, `routes/recall.rs`), graceful shutdown is wired (`main.rs:92
with_graceful_shutdown`), tracing middleware is per-route. Two gaps: `routes/healthz.rs:11` returns
`{"ok":true}` with **no storage probe** (a dead Moon still answers 200 → defeats rollout auto-cutback),
and `lunaris_eval_score` is registered but always 0 (populated only by an out-of-process binary).

**4 · correctness-security (ahead)** — INGEST-04 single `atomic_write` at
`pipeline.rs:assemble_and_write`; `IngestBody`/`RecallRequest`/`ForgetRequestDto` carry
`#[serde(deny_unknown_fields)]` (`dto.rs:34`); `Scope` re-validates on every wire deserialize
(`scope.rs:80`); migration `20260511000006_rls_with_check.sql` adds WITH CHECK parity; every Postgres
read path calls `set_config('lunaris.scope',…)` (incl. `keyword.rs:110` — the RC-A regression is
patched). Mem0 has app-layer-only isolation and an open graph-delete data-retention bug (#3245).

**5 · memory-update-intelligence (gap, contested)** — the production ingest
(`pipeline.rs:assemble_and_write`) is purely additive: no read of existing memories, no contradiction
detection, no DELETE WriteOp variant. `ActRConsolidator::consolidate` (`act_r.rs:244`) archives on
recency-weighted activation, not contradiction. `invalidate.rs` is a bulk time-range hook, not
per-observation reconciliation. Structural blake3 `EntityId` dedup exists at the graph layer only.

**6 · multi-level-memory** — `scope.rs:Scope` is an opaque `[A-Za-z0-9_\-.]{1,128}` string with no
hierarchy. `RetrievalBuilder::filter` (`builder.rs`) wires metadata filtering at recall, so a caller who
stamps `category` at ingest *can* filter on it — but there is no typed user/session/agent level and no
auto-categorization pass.

**7 · graph-quality (ahead)** — structured ingest always writes the graph
(`structured_ingest.rs:ingest_structured_inner`), the LLM-extract graph path fans GraphNode/GraphEdge
into the same atomic write (`ingest.rs:466-514`, behind an off-by-default toggle), and recall fuses
`graph_traverse_decayed` + `FT.NAVIGATE` into RRF (`operators/navigate.rs`, `operators/graph.rs`) with
**no LLM on the read path**. Open gap: RAPTOR community edges are a `TODO(phase-future)`.

**8 · sdk-dx** — `lunaris-py` (`pip install lunaris`) and `lunaris-ts`
(`npm i @pilotspace/lunaris`) ship full surfaces; `lunaris-mcp` ships 17 tools as a universal shim. But
LangGraph/CrewAI/Letta adapters **do not exist** — the only code mention is a conformance test corpus
string — while `POSITIONING.md:105` advertises them as a shipped "v0.4 ecosystem" (the repo is at v0.7).

## D. Accuracy & latency bench

**Mem0 published numbers (self-reported, methodology noted, NOT independently replicated):**
- LoCoMo: **92.5** (research page) / 91.6 (blog) — single-pass v3 algorithm; ~6,956 mean tokens/retrieval.
  Methodology: Mem0's own open-source eval framework; the 92.5↔91.6 discrepancy is an unexplained run/methodology variance flag.
- LongMemEval: **94.4** (research) / 93.4 (blog). BEAM-10M drops to 48.6 (temporal reasoning 16.3) — scale weak spot.
- ECAI-2025 paper: "**26% relative LLM-as-a-Judge improvement over OpenAI memory**" on LoCoMo (66.9% vs 52.9%);
  "**91% lower p95 latency**" (1.44 s vs 17.12 s full-context); ">90% token savings". Methodology: LoCoMo, LLM-as-a-Judge, full-context baseline.

**Lunaris numbers (real, manually collected — NOT CI-gated; like-for-like methodology noted):**
- Retrieval-only p50 **3.1 ms** / p99 3.6 ms (SQuAD 3k×300, Moon v0.3.0, Q4 GGUF). Methodology:
  retrieval-only noop-embed pass (`docs/benchmarks/v0.7-moon-v030-rerun.md`).
- Target-corpus envelope p50 **19.2–22.4 ms** / p95 22.3–24.1 / p99 23.4–24.4 ms (100k docs per
  scope, single-shard Moon v0.8.5, Apple M4 Pro, graph OFF, rerank OFF, k=30, retrieval-only,
  500 timed queries after 50 warmup, ± 3 ms run-to-run p50 drift) — meets the 25 ms contract
  with ≤ 25 % headroom. Methodology + raw samples: [`docs/operations/capacity.md`](../operations/capacity.md).
  *(The former "strict-replay p50 10.3 ms" line was retracted 2026-08-21: it was measured on
  Ollama + EmbeddingGemma 300M at k=3, a stack deleted in v0.4 and again in v0.6.)*

**Apples-to-oranges caveat (mandatory):** Mem0's headline numbers are **accuracy** on LoCoMo/LongMemEval;
Lunaris's real numbers are **retrieval latency** on SQuAD. They measure different axes and **cannot be
compared directly**. Lunaris currently has **no comparable accuracy number** because the LoCoMo/LongMemEval
harnesses are stubbed — closing that (task `eval-gauntlet-ci-gate`) is the prerequisite for any honest
accuracy claim. This is flagged, not a like-for-like comparison.

**Sharper than "stubbed" (2026-08-21 audit):** ER-F1 is a literal stub returning `0.0`
(`crates/lunaris-bench/src/eval/er_f1.rs`). LoCoMo is worse than a stub — it is a
**self-retrieval tautology**: `eval/locomo.rs` ingests each question's own gold answer
into a scratchpad (`eval/mod.rs::ingest_answers_to_pad`) and then greps for it
(`recall_j_score_from_pad`). Any J-score it emits measures string round-tripping, not
memory. Both harnesses emit `SKIPPED` on every failure path, so neither can go red, and
the `eval-gauntlet.yml` workflow that ran them has been deleted from `main` (it failed
200/200 runs at 0 s duration before that). **Never publish a figure from either.**

## E. Ranked backlog (ROI-ordered within severity; P0 anchor = production-risk + core contract)

| proposed_task_slug | dimension | severity | impact | acceptance_evidence | rough_effort | depends-on |
|--------------------|-----------|----------|--------|---------------------|--------------|------------|
| io-failsafe-wiring | reliability | P0 | removes the only unbounded-stall on the write path; activates already-tested resilience | fault-injected stalled Moon HSET returns an error within a deadline (not a hang); production open() builds a breaker-wrapped extractor — discriminating test green | M | none |
| eval-gauntlet-ci-gate | eval | P1 | unlocks the only honest accuracy claim vs Mem0; makes the "quality gate" real | LoCoMo/LongMemEval/ER-F1 emit real non-zero scores against live Moon; CI fails on a seeded sub-threshold regression | L | none |
| observability-rollout-maturity | observability | P1 | makes the 5%→100% rollout auto-cutback trustworthy (high ROI: S effort) | /healthz returns 503 when Moon is unreachable (probe test); lunaris_eval_score reflects the last eval run | S | none |
| memory-update-intelligence | memory-update-intelligence | P1 | memories converge instead of growing monotonically; stale facts stop surfacing at recall | ingesting a contradicting fact supersedes/dedupes the prior — recall returns the new, not both (discriminating test) | L | none |
| multi-level-memory-categories | multi-level-memory | P1 | Mem0-compatible ergonomics for multi-agent/multi-user platforms (LangGraph/CrewAI patterns) | typed user/session/agent levels + category filter exercised end-to-end via the SDK | M | none |
| sdk-integrations-dx | sdk-dx | P1 | unblocks adoption from the dominant Python agent frameworks; fixes a false shipped-docs claim | a LangGraph adapter example runs against lunaris-server; POSITIONING.md adapter claim matches shipped code | M | none |
| graph-quality-parity | graph-quality | P2 | turns the graph "ahead" story from asterisked to clean | graph traversal contributes to default-recall RRF in a discriminating test; RAPTOR community edges navigable | S | none |
| correctness-security-harden | correctness-security | P2 | locks in the lead; prevents a future unscoped-write regression on the Moon path | an unscoped Moon write attempt is rejected in a test; the Moon scoping invariant is documented | S | none |

**Wave recommendation (ROI):** ship **io-failsafe-wiring** (P0) first; then the two **S-effort P1s**
(`observability-rollout-maturity`) and the highest-leverage **L** (`eval-gauntlet-ci-gate`, the
accuracy-proof prerequisite). `memory-update-intelligence` (L) is the biggest true Mem0-parity build —
schedule it as its own wave.

## F. Reconciliation — shipped competitive docs vs reality

| existing_doc | prior_claim | status | note |
|--------------|-------------|--------|------|
| docs/MIGRATING-FROM-MEM0.md | Mem0 has no bi-temporal model (overwrite semantics) | confirmed | Mem0 exposes no valid-time API; holds |
| docs/MIGRATING-FROM-MEM0.md | Mem0 atomicity is best-effort per-store, no cross-store txn | confirmed | Mem0 writes vector+relational+graph in separate calls |
| docs/MIGRATING-FROM-MEM0.md | Mem0 recall latency 200–500 ms | corrected | imprecise — Mem0 publishes p95 1.44 s (selective) with a wide query-dependent range; cite the dated figure |
| docs/MIGRATING-FROM-MEM0.md | Lunaris recall p50 ~15 ms vs Mem0 ~300 ms | corrected twice | 2026-07: re-cited to 10.3 ms strict-replay. **2026-08-21: that too was retracted** (deleted Ollama/candle stack, k=3, 10k corpus). Current figure is the GA-2b envelope p50 19.2–22.4 ms @ 100k docs (manual bench, not CI-gated); the Mem0 ~300 ms remains unsourced — Mem0's only published figure is p95 ~1.44 s |
| docs/book/src/getting-started/why-lunaris.md | Mem0 graph: n/a | corrected | Mem0g exists (OSS v3 removed it → Platform-only, with open bugs); "n/a" is wrong |
| docs/POSITIONING.md | v0.4 ecosystem — LangGraph / CrewAI / Letta adapters | corrected | not shipped at v0.7 — the adapters do not exist in code; mark roadmap, not delivered |
| docs/POSITIONING.md | "memory + chat agent in 5 minutes → use Mem0" (DX honesty) | confirmed | Mem0's pip-install quickstart DX advantage is real |
| docs/book/src/getting-started/why-lunaris.md | Mem0 multi-tenancy = user_id string (no type validation) | confirmed | Mem0 uses untyped user_id/agent_id/run_id params |
