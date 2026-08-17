# ADR: OTLP export is post-GA — v0 observability is Prometheus + correlation-ID JSON logs

- **Date**: 2026-08-17
- **Status**: Accepted (owner-approved de-scope, GA-3)
- **Owners**: Lunaris core
- **Related**: `docs/operations/observability.md`,
  `docs/operations/slo.md`,
  `crates/lunaris-server/src/middleware/tracing.rs` (correlation IDs),
  `crates/lunaris-server/src/metrics.rs` (Prometheus catalogue)

## Context

The GA operability review asked whether Lunaris should ship OpenTelemetry
(OTLP) trace/metric export before GA. The current, code-verified state:

- **Nothing in `crates/` speaks OTLP.** There is no `opentelemetry` /
  `tracing-opentelemetry` dependency anywhere in the workspace. The only
  mention is a comment in `crates/lunaris-verify/src/worker.rs:208` noting
  W3C trace-context propagation as "a v1 enhancement".
- What v0 **does** ship is complete for single-service operation:
  - Prometheus text exposition at `/metrics` — a 14-metric catalogue
    covering verbs, latency histograms, queues, resilience, and readiness
    (`docs/operations/observability.md` §2), now with SLO burn-rate rules
    on top (`docs/operations/slo.md`).
  - Structured JSON logs in production (`LUNARIS_ENV=production`) with a
    per-request `correlation_id` span field, read from or minted into the
    `x-correlation-id` header (`middleware/tracing.rs`, CONTEXT.md D-24) —
    so cross-service correlation is possible today via header propagation
    and log joins, without OTLP.
- **vendor/moon reserves an empty `otel` feature** (`vendor/moon/Cargo.toml`,
  `otel = []`) — a placeholder with no implementation behind it. Moon-side
  spans are not available to join against even if Lunaris exported traces.

## Decision

**De-scope OTLP from GA.** v0 observability is Prometheus metrics plus
correlation-ID JSON logs, and that is the documented, supported surface.
OTLP (traces first, metrics maybe never — Prometheus is already the metrics
contract) is revisited **post-GA**.

## Why

1. **Lunaris is one process talking to one Moon.** Distributed tracing pays
   off across many hops; the recall path's hop count is exactly the thing
   the architecture minimizes (`docs/book/src/guides/recall-anatomy.md`).
   Intra-process stage timing is already visible in the Prometheus
   histograms, and cross-service joins work via `x-correlation-id`.
2. **The substrate cannot participate yet.** With Moon's `otel` feature an
   empty reservation, a Lunaris-side OTLP exporter would produce traces
   that dead-end at the storage boundary — most of the latency mystery an
   operator would reach for traces to solve.
3. **GA capacity is finite and the SLO work is the higher-leverage
   observability investment** (burn-rate alerts landed in GA-3; the GA-2
   capacity study is still open). Adding an OTLP pipeline now adds a
   dependency tree (`opentelemetry`, `tonic`/http exporters) and a config
   surface without retiring any GA risk.

## Consequences

- Operators needing traces today: propagate `x-correlation-id` from your
  edge, ship the JSON logs, and join on the field. This is documented in
  `docs/book/src/operations/security.md` (logs checklist item) and
  `docs/operations/observability.md` §6.
- The `lunaris-verify` worker comment stands: W3C trace-context header
  propagation is the natural first slice when this is revisited.
- Revisit trigger, not a date: OTLP re-enters planning when (a) a
  downstream platform (Helios) requires trace joins across services, or
  (b) Moon's `otel` feature gains an implementation to join against —
  whichever comes first, and in any case not before GA closes.
