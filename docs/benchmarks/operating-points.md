# The two operating points — `fast` and `quality`

*Settled 2026-08-21 (owner decision W3.2, `docs/planning/2026-08-21-ship-plan.md`).*

Lunaris ships one recall root (`lunaris_retrieve::production_root`) that
runs in **two configurations**. Both are real, both are measured, and they
differ by roughly **two orders of magnitude in latency**. Publishing a
quality number from one and a latency number from the other — without
saying so — is the single defect this page exists to kill.

**The rule: every published number states its operating point.** A table
row, a README line, a changelog entry, a baseline JSON, a raw artifact
envelope. No exceptions. A number with no operating point is not
publishable; retract it rather than guess which one it was.

---

## Definitions

| | **`fast`** | **`quality`** |
|---|---|---|
| Cross-encoder rerank | **OFF** | **ON** |
| How it is selected | the default — `LUNARIS_RECALL_RERANK` unset | `LUNARIS_RECALL_RERANK=1` |
| Shipped default? | **yes** | no — opt-in |
| Extra weights needed | none | `bge-reranker-v2-m3.Q5_K_M.gguf` |
| Rerank depth | n/a | `LUNARIS_RECALL_RERANK_TOP_IN`, default `2k` |

Source of truth for the toggle:
[`crates/lunaris/src/recall_rerank.rs`](../../crates/lunaris/src/recall_rerank.rs)
— `RecallRerankConfig::from_values` treats exactly
`"1" \| "true" \| "TRUE" \| "on" \| "ON"` as ON; unset is OFF, and the
config is frozen at handle construction.

`lunaris-hook`'s session-context injection is pinned to `fast` on purpose
(`crates/lunaris-hook/src/context.rs:65`) — a ~1.3 s stage cannot sit on a
session-start hook.

### The graph toggle is a third axis, not a third operating point

`LUNARIS_RECALL_GRAPH` (fact/entity legs) is orthogonal to rerank and is
also default-OFF. Where it matters, say so explicitly
(`fast, graph ON`). The GA-2b measurement of `fast, graph ON` is
**p50 39.1 ms** — roughly double `fast` (`docs/operations/capacity.md` §3
row (c)).

---

## Measured latency — the same corpus, both points

100,000 documents per scope, single-shard Moon v0.8.5, Apple M4 Pro,
k = 30, graph OFF, retrieval-only decomposition (query embedding excluded
via `StubEmbedder`). Full methodology and environment:
[`docs/operations/capacity.md`](../operations/capacity.md) §1–§3. Raw
envelopes: [`ga2b-raw/`](ga2b-raw/).

| Operating point | mean | **p50** | p95 | **p99** | max | n | raw artifact |
|---|---|---|---|---|---|---|---|
| **`fast`** — run 1 | 19.5 | **19.2** | 22.3 | **23.4** | 24.0 | 500 | `ga2b-raw/baseline-run1.json` |
| **`fast`** — run 2 (later, disjoint queries) | 22.2 | **22.4** | 24.1 | **24.4** | 24.6 | 500 | `ga2b-raw/baseline-run2.json` |
| **`quality`** — `top_in = 60` (the default `2k`) | 1309.5 | **1301.3** | 1367.0 | **1510.7** | 2585.0 | 500 merged | `ga2b-raw/rerank-top60-{a,b}.json` |
| **`quality`** — `top_in = 30` | 580.9 | **575.6** | 598.9 | **996.7** | — | 100 | `ga2b-raw/rerank-top30.json` |

All figures in ms, engine-side. Run-to-run p50 drift on the `fast` point
was **± 3 ms** on a non-clean dev box; treat any `fast` p50 delta under
about 3 ms as noise.

**The 25 ms p50 contract is a `fast`-path contract.** It holds there with
≤ 25 % headroom. It does not hold, and was never claimed to hold, on
`quality`: a deployment that turns rerank on must re-derive its own
latency SLO (`docs/operations/capacity.md` §4).

---

## Which point each published benchmark ran at

| Benchmark | Operating point | Where |
|---|---|---|
| GA-2b capacity envelope | **both**, measured separately | `docs/operations/capacity.md` |
| PersonaMem 32k (75.0 % single-reader) | **`quality`** — `PM_RERANK=1` in `scripts/bench/pm/run_pm.sh:112` | `scripts/bench/pm/RESULTS.md` |
| LongMemEval CI recall ratchet | **`quality`** — `RERANK` defaults to `1` in `anygold_gate.sh` | `scripts/bench/lme/baselines/` |
| LongMemEval N=125 A/B runner | **`quality`** — `run_lme.sh` sets rerank on | `scripts/bench/lme/` |
| §5 rerank drift regression baseline | **`quality`** by construction (it measures the reranker) | `crates/lunaris-llamacpp/tests/section5_rerank_parity.rs` |

Read that table twice. **Every recall-quality number Lunaris publishes was
measured on `quality`, and every latency number that meets the 25 ms
contract was measured on `fast`.** That is the exact split the two-point
decision exists to make visible, and it is why the CI ratchet's operating
point is itself a live defect — see
[`scripts/bench/lme/baselines/README.md`](../../scripts/bench/lme/baselines/README.md).

---

## How to name them in prose

- Good: "p50 19.2–22.4 ms on the **fast path** (rerank OFF, the shipped
  default), 100k docs/scope."
- Good: "75.0 % on PersonaMem 32k, **quality path** (rerank ON),
  claude-sonnet-5 reader, exact letter match."
- Bad: "p50 ~20 ms and 75 % on PersonaMem." Two points, one sentence, no
  labels — this is the defect.
