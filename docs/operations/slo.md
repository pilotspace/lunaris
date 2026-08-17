# Service Level Objectives — `lunaris-server`

This document defines the SLIs, SLO targets, error-budget policy, and
burn-rate alert math for a production `lunaris-server` deployment. The
thresholds shipped in
[`deploy/prometheus/lunaris-alerts.yml`](../../deploy/prometheus/lunaris-alerts.yml)
are derived from **this document** — change them here first, then in the
rules file.

Companion docs: [`observability.md`](observability.md) (metric catalogue,
scrape config), [`external-moon.md`](external-moon.md) (deployment runbook),
[`backup-restore.md`](backup-restore.md) (durability; RPO/RTO evidence).

> **Envelope status.** The GA-2b capacity study
> ([`capacity.md`](capacity.md)) measured the recall envelope at the stated
> **target corpus of 100k documents** (single-shard Moon, retrieval-only
> decomposition): default-config p50 19–22 ms / p99 23.4–24.4 ms. The
> latency rows below are now anchored to that measurement. Two caveats
> survive: (1) the product line "millions of bi-temporal facts" is NOT
> measured — 100k is the validated envelope, and the observed scaling trend
> says millions will not meet 25 ms p50 without further storage-side work
> ([`capacity.md` §5](capacity.md)); (2) the ingest-availability row still
> has no measured availability baseline and stays provisional.

---

## 1. SLIs — what we measure

All SLIs are computed from metrics `lunaris-server` actually emits
(catalogue: [`observability.md` §2](observability.md)). Server-side
measurement: the histograms observe end-to-end handler latency at the HTTP
layer (`routes/recall.rs`, `routes/ingest.rs`), *inside* any reverse proxy
you deploy in front.

| SLI | Definition (PromQL sketch) |
|---|---|
| **Recall availability** | `sum(rate(lunaris_recall_total{status="ok"}[W])) / sum(rate(lunaris_recall_total[W]))` |
| **Recall latency (fast-request ratio)** | `sum(rate(lunaris_recall_duration_seconds_bucket{le="0.1"}[W])) / sum(rate(lunaris_recall_duration_seconds_count[W]))` |
| **Ingest availability** | `sum(rate(lunaris_ingest_total{status="ok"}[W])) / sum(rate(lunaris_ingest_total[W]))` |
| **Serving readiness** | `avg_over_time(lunaris_ready[W])` (valid only while something polls `/readyz` — see the `LunarisReadinessGaugeStale` alert) |

Two boundaries worth knowing before arguing with the numbers:

* **Shed and timed-out requests are not in the verb counters.** A `503` from
  the concurrency limiter or a `408` from the timeout layer never reaches a
  route, so it lands in `lunaris_http_shed_total` / `lunaris_http_timeout_total`,
  not in `lunaris_recall_total{status="error"}`
  ([`observability.md` §2 "Not incremented anywhere"](observability.md)).
  The availability SLIs above therefore measure *served* requests; sustained
  shedding is caught by its own alerts, and a shed-ratio SLI is deliberately
  not defined (no total-arrivals counter exists — same section).
* **The buckets are the stock Prometheus defaults.** `metrics.rs` registers
  the histograms without custom buckets, so the boundaries include `0.025`
  and `0.1` — both SLI thresholds below are exact bucket edges, not
  interpolations.

## 2. SLO targets

Window for all objectives: **30 days rolling**.

| Objective | Target | Budget (30 d) | Status |
|---|---|---|---|
| Recall availability | **99.9 %** of served `/v1/recall` requests return `status="ok"` | 0.1 % of requests | active |
| Recall latency | **99 %** of served recalls complete in **≤ 100 ms** server-side | 1 % of requests | active — anchored by GA-2b: engine-side p99 23.4–24.4 ms @ 100k docs, ~4× headroom for the HTTP layer ([`capacity.md` §3](capacity.md)). **Void with `LUNARIS_RECALL_RERANK=1`** — the rerank stage measures ~1.3 s/recall; rerank deployments must re-derive this row ([`capacity.md` §4](capacity.md)) |
| Recall p50 contract (KPI) | p50 **≤ 25 ms** — the product contract; tracked on dashboards via the `le="0.025"` bucket ratio (≥ 50 % of requests ≤ 25 ms), **not paged** | — | **holds at the 100k target corpus** (GA-2b: p50 19–22 ms, ≤ 25 % headroom; engine-side, default config — graph-ON measures ~39 ms and does not fit under this KPI). Beyond 100k the contract is unvalidated — see [`capacity.md` §5](capacity.md) |
| Ingest availability | **99.9 %** of served `/v1/ingest` requests return `status="ok"` | 0.1 % of requests | **provisional** — still no measured availability baseline. GA-2b observed sustained ~1.2 k docs/s build-rate ingest with two retryable Moon pressure states worth knowing about ([`capacity.md` §5](capacity.md)) |

### Why 100 ms for the paging latency SLO

The measured engine baseline is **p50 10.3 ms / p99 20.8 ms** (strict
replay, 10k-doc corpus, live Moon —
[`docs/benchmarks/v0.2.x/README.md`](../benchmarks/v0.2.x/README.md), cited
in [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md) "The numbers that anchor the
claims"). 100 ms is that measured p99 with ~5× headroom for the HTTP layer,
reranking, and corpus growth beyond the measured envelope. It is a *paging*
threshold, deliberately looser than the 25 ms product contract: the contract
is a KPI we track and defend in benchmarks; the SLO is what we wake someone
up for.

GA-2b measured the p99 at the 100k target corpus: **23.4–24.4 ms**
engine-side ([`capacity.md` §3](capacity.md)) — materially above the 10k
baseline's 20.8 ms but still ~4× under the 100 ms threshold, so the target
stands for the default config. Two conditions would force a revisit:
corpus growth past the 100k envelope (the p50 KPI, not this SLO, breaks
first), and **enabling the opt-in rerank stage**, which measures ~1.3 s per
recall at its default depth and makes this row unmeetable as written
([`capacity.md` §4](capacity.md)).

## 3. Error-budget policy

Budget accounting is per-objective over the 30-day window.

| Budget consumed (30 d) | Action |
|---|---|
| < 50 % | Normal operations. |
| ≥ 50 % | Ticket (see burn-rate ticket alerts): investigate within one business day; the on-call review names a cause. |
| ≥ 90 %, or any page-level burn alert | Page. Feature rollouts to the affected deployment **freeze** (including progressive-rollout stage promotions) until the burn is diagnosed and the budget projection is back under 100 %. |
| 100 % (budget exhausted) | Freeze holds; only reliability changes ship. A post-incident review is mandatory before the freeze lifts. |

A deliberate load-shedding episode (`LunarisSheddingLoad` firing while
availability SLIs stay green) does **not** count against the budget — that is
the server protecting itself as designed. It does count as a capacity signal
and should produce a scaling ticket.

## 4. Burn-rate alerts

Multiwindow, multi-burn-rate (the standard SRE-workbook pattern). Burn rate
= (error ratio over window) ÷ (budget ratio). Both windows must exceed the
threshold simultaneously — the long window gives significance, the short
window makes the alert stop when the burn stops.

| Tier | Burn rate | Long window | Short window | Budget consumed if sustained | Severity |
|---|---|---|---|---|---|
| Page | 14.4× | 1 h | 5 m | 2 % of the 30-day budget per hour | `critical` |
| Ticket | 6× | 6 h | 30 m | 5 % per 6 h | `warning` |

Applied to:

* **Recall availability** (budget ratio 0.001): pages at a sustained error
  ratio > 1.44 %, tickets at > 0.6 %.
* **Ingest availability** (budget ratio 0.001): same thresholds.
* **Recall latency** (budget ratio 0.01, "error" = request slower than
  100 ms): pages when > 14.4 % of requests are slow, tickets at > 6 %.

The exact PromQL lives in
[`deploy/prometheus/lunaris-alerts.yml`](../../deploy/prometheus/lunaris-alerts.yml)
(group `lunaris-slo-burn`) and is generated from the table above:
`error_ratio(long) > burn × budget AND error_ratio(short) > burn × budget`.
The static `LunarisRecallLatencyHigh` / `LunarisRecallErrorRate` rules remain
as slow-drift backstops; the burn-rate rules are the primary SLO alerts.

Deployments that promise a different SLO than §2 must scale the thresholds
in the rules file accordingly — nothing self-adjusts (no metric exports the
SLO, just as none exports `LUNARIS_HTTP_CONCURRENCY`).

## 5. Measurement provenance — where every number comes from

| Number | Source | Caveat |
|---|---|---|
| p50 10.3 ms / p99 20.8 ms recall | `docs/benchmarks/v0.2.x/README.md` (strict replay, 10k docs, live Moon) | measured envelope is 10k docs, not the production target corpus |
| Sub-25 ms p50 contract | `docs/ARCHITECTURE.md` — the product's core value statement | a *contract to defend*, not a measured ceiling at every corpus size |
| p50 3.1 ms / p99 3.6 ms (Moon v0.3.0 rerun) | `docs/benchmarks/v0.7-moon-v030-rerun.md` | 3k-doc corpus — smaller than the 10k baseline; see that report's caveat |
| 99.9 % availability targets | policy choice (one 43-minute full-outage budget per 30 d), **not** a measured baseline | provisional; revisit with GA-2 production data |
| 100 ms latency SLO threshold | derived: measured 20.8 ms p99 × ~5 headroom, rounded to a stock bucket edge | provisional until GA-2 capacity study |
| RPO = 0 / RTO < 1 s (Moon durability) | `docs/operations/backup-restore.md` restore drill | informs the readiness objective, not a request-level SLI |
| p50 19–22 ms / p99 23.4–24.4 ms @ 100k docs (default config) | [`capacity.md`](capacity.md) (GA-2b, Moon v0.8.5, M4 Pro, retrieval-only) | the target-corpus envelope; two runs quoted because run-to-run p50 drift was ± 3 ms on a non-lab box |
| Rerank stage ~1.3 s/recall (top_in=60), ~1.0–1.4 s one-time lazy load | [`capacity.md` §4](capacity.md) | opt-in `LUNARIS_RECALL_RERANK`; linear in `top_in`; voids the 100 ms latency row where enabled |

There is **no measured number** behind the ingest-availability target
(GA-2b measured build-rate throughput, not availability), and no measured
envelope beyond 100k documents. Do not quote the "millions of facts"
product line as measured — [`capacity.md` §5](capacity.md).
