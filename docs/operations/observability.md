# Observability — `/metrics`, scraping, and a starter alert set

Every metric named here was read out of
[`crates/lunaris-server/src/metrics.rs`](../../crates/lunaris-server/src/metrics.rs)
at the cited line. Nothing is aspirational — if a series is not in the table
below, `lunaris-server` does not emit it today.

Companion runbooks: [`external-moon.md`](external-moon.md) (deployment),
[`backup-restore.md`](backup-restore.md) (durability),
[`slo.md`](slo.md) (SLO targets, error-budget policy, burn-rate math).

---

## 1. The endpoint

`GET /metrics` — Prometheus text format
(`text/plain; version=0.0.4; charset=utf-8`), mounted on the **root** router,
**no authentication and no rate limit** (`lib.rs:270`,
`routes/metrics.rs:12-17`). It sits alongside `/healthz` and `/readyz`.

> **You must restrict it yourself.** The route is deliberately unauthenticated
> so a scraper needs no bearer token (standard Prometheus convention). Put it
> behind a network ACL, a reverse proxy, or a private listener. The metric
> labels include the tenant/scope name, so an open `/metrics` leaks your tenant
> roster.

`--metrics-disabled` (`config.rs:97-98`) makes the route answer **404**
— deliberately, so a scraper sees a clean disable rather than an empty body
that looks like a stale exporter (`routes/metrics.rs:8-10`).

The registry is force-initialized on the first scrape, so a freshly-started
process serves the full catalogue at zero values rather than an empty body
(`routes/metrics.rs:35-38`).

### What is *not* there

The workspace pins `prometheus = { version = "0.14", default-features = false }`
(`Cargo.toml:218`), which **disables the process collector**. There are no
`process_cpu_seconds_total`, `process_resident_memory_bytes`, or
`process_open_fds` series. Get RSS/CPU from your node exporter or container
runtime, not from Lunaris.

There is also **no** storage-operation latency histogram and **no** circuit-breaker
state gauge. Moon-side latency is visible only indirectly (through
`lunaris_*_duration_seconds` and the readiness canary) or from Moon's own
admin `/metrics` (§3).

---

## 2. The catalogue — 14 metrics

| Metric | Type | Labels | Emitted by | metrics.rs |
|---|---|---|---|---|
| `lunaris_ingest_total` | counter | `tenant`, `status` | `routes/ingest.rs:127` | `:98` |
| `lunaris_ingest_duration_seconds` | histogram | `tenant` | `routes/ingest.rs:95` | `:104` |
| `lunaris_recall_total` | counter | `tenant`, `mode`, `status` | `routes/recall.rs:187` | `:110` |
| `lunaris_recall_duration_seconds` | histogram | `tenant`, `mode` | `routes/recall.rs:54` | `:116` |
| `lunaris_forget_total` | counter | `tenant`, `target_kind`, `hard` | `routes/forget.rs:120` | `:122` |
| `lunaris_verify_queue_depth` | gauge | `topic` | `queue_depth_poller.rs:84` (10 s) | `:128` |
| `lunaris_consolidator_queue_depth` | gauge | `topic` | `queue_depth_poller.rs:91` (10 s) | `:134` |
| `lunaris_error_total` | counter | `kind` | `middleware/error.rs:25` | `:140` |
| `lunaris_eval_score` | gauge | `harness` | `eval_score.rs:48` | `:146` |
| `lunaris_hotkey_samples` | gauge | `scope`, `kind` | `hotkeys_poller.rs:134` (10 s) | `:152` |
| **`lunaris_http_in_flight`** | gauge | *(none)* | `middleware/resilience.rs:69/76` | `:160` |
| **`lunaris_http_shed_total`** | counter | *(none)* | `middleware/resilience.rs:163` | `:166` |
| **`lunaris_http_timeout_total`** | counter | *(none)* | `middleware/resilience.rs:114` | `:171` |
| **`lunaris_ready`** | gauge | *(none)* | `readiness.rs:135` | `:176` |

The four in bold are the 0.6.2 resilience/readiness additions.

### Label vocabularies (bounded on purpose)

From `metrics.rs:23-33`:

* `tenant` / `scope` — the `AuthClaims::tenant` set, i.e. your tokens file.
  Cardinality is operator-controlled; a tokens file with 10 000 entries gives
  you 10 000 series per metric.
* `mode` ∈ `{semantic, graph}`
* `status` ∈ `{ok, error}`
* `target_kind` ∈ `{id, scope, before}`; `hard` ∈ `{true, false}`
* `kind` (on `lunaris_error_total`) ∈ `{storage, validate, extract, retrieve,
  consolidate, unknown}` — capped at ~10 by `metrics.rs::error_kind`
* `topic` ∈ `{__lunaris_verify__, __lunaris_consolidate__}`
  (`queue_depth_poller.rs:34,39`)
* `harness` — a small static set, populated only when
  `LUNARIS_EVAL_RESULTS_PATH` points at an eval-results JSON
  (`eval_score.rs:76`); otherwise the gauge stays at 0.

### Reading the four new ones correctly

* **`lunaris_ready`** is `1` iff the last `/readyz` probe passed **all three**
  checks: storage PING, a `KvPut`+`KvDelete` **write canary** on
  `lunaris:__health__:canary`, and an embedder-configured check — each bounded
  at 2 s (`readiness.rs:44-49, 143-195`). Results are cached for 5 s and
  single-flighted, so the gauge only moves when a probe actually runs; if
  nothing polls `/readyz`, the gauge is stale. **Scrape or probe `/readyz`.**
* **`lunaris_http_shed_total`** increments when the concurrency limit rejects
  an arrival with `503` + `Retry-After: 1` (`resilience.rs:55, 163`). Requests
  are shed, never queued — a non-zero rate means the server is protecting
  itself, which is correct behaviour and a capacity signal.
* **`lunaris_http_timeout_total`** increments on `408`, when a request exceeds
  `--http-timeout-secs` (`resilience.rs:114`). The budget covers producing the
  response, not streaming an SSE body.
* **`lunaris_http_in_flight`** is maintained by an RAII guard that decrements
  on `Drop`, so a client disconnect or an abandoned timeout cannot leak it
  upward (`resilience.rs:61-78`). The bounded shutdown path reads it to report
  `aborted_in_flight` when the drain deadline expires (`shutdown.rs:156`).
  **There is no metric exporting the configured limit** — you must template
  `LUNARIS_HTTP_CONCURRENCY` into any saturation rule (§5).

### Not incremented anywhere

`lunaris_error_total` only counts errors that pass through
`middleware/error.rs::map_error`. Errors handled inside a route (or a 503 from
load shedding, or a 408 from the timeout layer) do **not** land here — use the
dedicated shed/timeout counters and the `status="error"` label on the verb
counters instead.

---

## 3. Prometheus scrape config

Drop-in, ready to use. Also shipped as
[`deploy/prometheus/prometheus.yml`](../../deploy/prometheus/prometheus.yml).

```yaml
global:
  scrape_interval: 15s
  evaluation_interval: 15s

rule_files:
  - /etc/prometheus/lunaris-alerts.yml

scrape_configs:
  # ── lunaris-server ────────────────────────────────────────────────────────
  - job_name: lunaris-server
    metrics_path: /metrics
    static_configs:
      - targets: ["lunaris-server:8080"]
        labels:
          service: lunaris

  # ── readiness as a blackbox-style probe ───────────────────────────────────
  # /readyz runs the WRITE CANARY, and `lunaris_ready` only updates when a
  # probe fires. Scraping /metrics alone leaves the gauge stale, so poll
  # /readyz too. Requires the blackbox_exporter with an `http_2xx` module.
  - job_name: lunaris-readyz
    metrics_path: /probe
    params:
      module: [http_2xx]
    static_configs:
      - targets: ["http://lunaris-server:8080/readyz"]
    relabel_configs:
      - source_labels: [__address__]
        target_label: __param_target
      - source_labels: [__param_target]
        target_label: instance
      - target_label: __address__
        replacement: blackbox-exporter:9115

  # ── Moon's own admin endpoint ─────────────────────────────────────────────
  # Enabled by `--admin-port 9100`; serves /healthz, /readyz and /metrics
  # (vendor/moon/src/admin/http_server.rs:138-148).
  - job_name: moon
    metrics_path: /metrics
    static_configs:
      - targets: ["moon:9100"]
        labels:
          service: moon
```

If you do not run a blackbox exporter, drop the `lunaris-readyz` job and
instead let your orchestrator's readinessProbe drive `/readyz` (Kubernetes
does exactly this) — the gauge then tracks the probe period. Alerting on
`up{job="moon"}` still works without it.

---

## 4. Which signal means what

| Question | Signal |
|---|---|
| Is the process alive? | `up{job="lunaris-server"}`, `/healthz` |
| Can it actually serve? | `lunaris_ready`, `/readyz` |
| Is Moon accepting **writes**? | `lunaris_ready == 0` with `checks.canary` non-`ok` in the `/readyz` body |
| Is Moon reachable at all? | `up{job="moon"}`; `lunaris_error_total{kind="storage"}` |
| Are we over capacity? | `lunaris_http_in_flight` vs `LUNARIS_HTTP_CONCURRENCY`; `rate(lunaris_http_shed_total[5m])` |
| Are requests too slow? | `rate(lunaris_http_timeout_total[5m])`, `lunaris_recall_duration_seconds` quantiles |
| Is the background work backing up? | `lunaris_verify_queue_depth`, `lunaris_consolidator_queue_depth` |

`/healthz` staying green while `/readyz` fails is the **designed** behaviour
for a downstream wedge: restarting the process does not un-wedge Moon, it just
adds a cold start (`routes/readyz.rs:9-14`). Alert on readiness; do not wire an
automatic restart to it.

---

## 5. Starter alert rules

Shipped as
[`deploy/prometheus/lunaris-alerts.yml`](../../deploy/prometheus/lunaris-alerts.yml).
**One value must be templated to your deployment** — the concurrency limit,
marked inline. The latency threshold and the burn-rate expressions come from
the SLO doc ([`slo.md`](slo.md) §2/§4); rescale them only if your deployment
promises a different SLO. The shipped file carries a second group,
`lunaris-slo-burn` (multiwindow burn-rate rules: page at 14.4×/1h+5m,
ticket at 6×/6h+30m — not reproduced below; see the file and `slo.md` §4).
The excerpt below shows the `lunaris` starter group.

```yaml
groups:
  - name: lunaris
    rules:
      # ── readiness ─────────────────────────────────────────────────────────
      - alert: LunarisNotReady
        expr: lunaris_ready == 0
        for: 2m
        labels: { severity: critical }
        annotations:
          summary: "lunaris-server {{ $labels.instance }} is not ready"
          description: >-
            /readyz has been failing for 2m. Curl it for the per-component
            breakdown: canary=timeout means Moon accepts connections but
            stalls writes (the wedge signature); ping=error means Moon is
            unreachable; embedder=error means no usable embedder is
            configured. Runbook: docs/operations/external-moon.md §10.

      - alert: LunarisReadinessGaugeStale
        expr: absent(lunaris_ready)
        for: 10m
        labels: { severity: warning }
        annotations:
          summary: "lunaris_ready is absent — nothing is polling /readyz"
          description: >-
            The gauge only updates when a /readyz probe runs. Add a
            readinessProbe or a blackbox job; otherwise LunarisNotReady can
            never fire.

      - alert: LunarisDown
        expr: up{job="lunaris-server"} == 0
        for: 2m
        labels: { severity: critical }
        annotations:
          summary: "lunaris-server scrape target is down"

      # ── storage ───────────────────────────────────────────────────────────
      - alert: MoonDown
        expr: up{job="moon"} == 0
        for: 2m
        labels: { severity: critical }
        annotations:
          summary: "Moon admin endpoint is unreachable"
          description: >-
            Moon is the only supported backend. Check the process, then
            `redis-cli -p 6379 PING` and
            `redis-cli -p 6379 INFO server | grep moon_version`.

      - alert: LunarisStorageErrors
        expr: rate(lunaris_error_total{kind="storage"}[5m]) > 0.1
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "Storage errors on {{ $labels.instance }}"
          description: >-
            Sustained StorageError rate. If this coincides with
            lunaris_ready == 0, treat it as a Moon outage, not an app bug.

      # ── load shedding / saturation ────────────────────────────────────────
      # TEMPLATE: replace 256 with your LUNARIS_HTTP_CONCURRENCY. No metric
      # exports the configured limit, so this threshold cannot self-adjust.
      - alert: LunarisInFlightSaturation
        expr: lunaris_http_in_flight > 0.8 * 256
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "In-flight requests above 80% of the concurrency limit"
          description: >-
            Sustained saturation precedes shedding. Either scale out, raise
            LUNARIS_HTTP_CONCURRENCY (which raises the worst-case backlog to
            concurrency x http-timeout-secs), or find the slow dependency.

      - alert: LunarisSheddingLoad
        expr: rate(lunaris_http_shed_total[5m]) > 0
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "lunaris-server is shedding requests (503 + Retry-After)"
          description: >-
            The concurrency limit is rejecting arrivals. This is correct
            self-protection, not a bug — but callers are being turned away.

      - alert: LunarisSheddingHeavily
        expr: rate(lunaris_http_shed_total[5m]) > 1
        for: 2m
        labels: { severity: critical }
        annotations:
          summary: "lunaris-server is shedding >1 req/s"

      - alert: LunarisRequestTimeouts
        expr: rate(lunaris_http_timeout_total[5m]) > 0.1
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "Requests exceeding --http-timeout-secs (408)"
          description: >-
            Usually a slow backend rather than a slow handler. Correlate with
            lunaris_recall_duration_seconds and Moon's own metrics.

      # ── latency ───────────────────────────────────────────────────────────
      # 0.1s = the paging latency SLO (slo.md §2, provisional until GA-2).
      # The 25ms p50 product contract is a dashboard KPI, not paged.
      - alert: LunarisRecallLatencyHigh
        expr: >-
          histogram_quantile(0.99,
            sum by (le, tenant) (rate(lunaris_recall_duration_seconds_bucket[5m]))
          ) > 0.1
        for: 10m
        labels: { severity: warning }
        annotations:
          summary: "p99 recall latency > 100ms (the SLO threshold) for tenant {{ $labels.tenant }}"

      - alert: LunarisRecallErrorRate
        expr: >-
          sum(rate(lunaris_recall_total{status="error"}[5m]))
            / clamp_min(sum(rate(lunaris_recall_total[5m])), 0.001) > 0.05
        for: 10m
        labels: { severity: warning }
        annotations:
          summary: "More than 5% of recalls are failing"

      # ── background work ───────────────────────────────────────────────────
      - alert: LunarisQueueBacklog
        expr: lunaris_verify_queue_depth > 10000 or lunaris_consolidator_queue_depth > 10000
        for: 15m
        labels: { severity: warning }
        annotations:
          summary: "Background queue {{ $labels.topic }} is backing up"
          description: >-
            The gauge stays at 0 on backends without queue support (the poller
            warns once at startup), so 0 is not proof of an empty queue.
```

### Rules deliberately NOT included

* **A shed-ratio rule** (`shed / (shed + served)`). There is no total-arrivals
  counter — `lunaris_*_total` counts *served* requests per verb, and shed
  requests never reach a route. Any ratio you build is an approximation.
* **A Moon write-stall rule keyed on Moon's own metrics.** Lunaris' `/readyz`
  canary is the validated detector for that failure mode; both production
  wedges this project has hit answered `PING` happily (`readiness.rs:3-16`).
* **Anything on `lunaris_hotkey_samples`.** It is a cumulative,
  1-in-64-sampled SpaceSaving *ranking* since Moon start — a pressure signal
  for humans, not a rate you can threshold (`metrics.rs:154-156`).

---

## 6. Logs

`lunaris::logging::init()` (`main.rs:28`) selects JSON when
`LUNARIS_ENV=production` **or** stdout is not a terminal; pretty otherwise.
Set `RUST_LOG` (e.g. `lunaris=info,lunaris_server=info`).

Log lines worth alerting on that have **no** metric counterpart:

| Line | Meaning |
|---|---|
| `readyz: write canary timed out — backend accepts connections but stalls writes` | the wedge signature (`readiness.rs:183`) |
| `moon: unsupported server version` | version handshake refusal at connect ([external-moon.md §3](external-moon.md)) |
| `hot_keys unsupported by backend` / `queue_depth unsupported by backend` | warn-once at startup; the corresponding gauge will stay empty |
| `aborted_in_flight` on shutdown | the drain deadline expired with requests still running (`shutdown.rs:156`) |
| a `NoopEmbedder` fallback banner at open | missing GGUF or a build without `lunaris/llamacpp` — vector recall will return zero rows ([external-moon.md §7-§8](external-moon.md)) |
