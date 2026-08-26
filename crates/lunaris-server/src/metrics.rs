//! Plan 05-05 OPS-06 — Prometheus metrics registry.
//!
//! ## Nine metrics per CONTEXT.md D-25 (verbatim)
//!
//! | Name                                | Type      | Labels                              |
//! |-------------------------------------|-----------|-------------------------------------|
//! | `lunaris_ingest_total`              | counter   | `tenant`, `status`                  |
//! | `lunaris_ingest_duration_seconds`   | histogram | `tenant`                            |
//! | `lunaris_recall_total`              | counter   | `tenant`, `mode`, `status`          |
//! | `lunaris_recall_duration_seconds`   | histogram | `tenant`, `mode`                    |
//! | `lunaris_forget_total`              | counter   | `tenant`, `target_kind`, `hard`     |
//! | `lunaris_verify_queue_depth`        | gauge     | `topic`                             |
//! | `lunaris_consolidator_queue_depth`  | gauge     | `topic`                             |
//! | `lunaris_error_total`               | counter   | `kind`                              |
//! | `lunaris_eval_score`                | gauge     | `harness`                           |
//! | `lunaris_hotkey_samples`            | gauge     | `scope`, `kind`                     |
//!
//! The tenth metric (`lunaris_hotkey_samples`, hotkeys-observability task)
//! extends the D-25 table: `scope` is bounded by the live tenant set (same
//! class as `tenant`); its `kind` is a CLOSED static set ≤ 13 enforced by
//! `hotkeys_poller::classify_hot_key`.
//!
//! ## Cardinality cap (T-05-05-02 mitigation)
//!
//! Every label has bounded set membership documented in CONTEXT.md D-25:
//! - `tenant` = `AuthClaims::tenant` set (operator-controlled tokens-file size).
//! - `mode` ∈ {`semantic`, `graph`}.
//! - `status` ∈ {`ok`, `error`}.
//! - `target_kind` ∈ {`id`, `scope`, `before`}.
//! - `hard` ∈ {`true`, `false`}.
//! - `kind` ∈ LunarisError variants (~6 values; capped at 10 — see [`error_kind`]).
//! - `harness` = a small static set populated by Plan 05-06 lunaris-evals.
//! - `topic` = the canonical `__lunaris_verify__` / `__lunaris_consolidate__` strings.
//!
//! ## Registry shape
//!
//! Uses `std::sync::OnceLock<Metrics>` for static-init lifetime (preferred
//! over `lazy_static!` per CLAUDE.md "Latest libraries policy" — std is
//! sufficient). The `register_*_vec!` macros from prometheus 0.14 register
//! with the global default registry, so `prometheus::gather()` later picks
//! up every metric without needing a custom registry handle.

#![forbid(unsafe_code)]

use std::sync::OnceLock;

use prometheus::{
    GaugeVec, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, register_gauge_vec,
    register_histogram_vec, register_int_counter, register_int_counter_vec, register_int_gauge,
    register_int_gauge_vec,
};

/// Container for the nine declared metrics. Lazily constructed via [`metrics`]
/// using `OnceLock::get_or_init` so a fresh process pays the registration cost
/// exactly once.
pub struct Metrics {
    pub ingest_total: IntCounterVec,
    pub ingest_duration: HistogramVec,
    pub recall_total: IntCounterVec,
    pub recall_duration: HistogramVec,
    pub forget_total: IntCounterVec,
    pub verify_queue_depth: IntGaugeVec,
    pub consolidator_queue_depth: IntGaugeVec,
    pub error_total: IntCounterVec,
    /// Plan 05-06 EVAL-* harness scores. Plan 05-05 declares the gauge so
    /// /metrics text format already lists the series at zero; Plan 05-06's
    /// lunaris-evals binary populates concrete values from eval-results.json.
    pub eval_score: GaugeVec,
    /// hotkeys-observability (10th metric) — sampled hot-key pressure per
    /// (scope, kind), fed by `hotkeys_poller`. Label cardinality: `scope` =
    /// live tenant set (same bound as `tenant`); `kind` is a CLOSED static
    /// set ≤ 13 (see `hotkeys_poller::classify_hot_key`). Raw key names
    /// NEVER appear — unparseable keys are dropped before labeling.
    pub hotkey_samples: IntGaugeVec,
    /// 0.6.2 P0-1 — HTTP requests currently inside the router, maintained by
    /// `middleware::resilience::in_flight_middleware`. Label-free (process
    /// scope) so it costs one series. `shutdown::serve_with_deadline` reads it
    /// when the grace window expires to report `aborted_in_flight`.
    pub http_in_flight: IntGauge,
    /// 0.6.2 P0-2 — requests rejected by the concurrency limit
    /// (`503` + `Retry-After`). A non-zero rate here is the load-shed signal:
    /// the server is protecting itself instead of queueing into an OOM.
    pub http_shed_total: IntCounter,
    /// 0.6.2 P0-2 — requests cut off by `--http-timeout-secs` (`408`).
    pub http_timeout_total: IntCounter,
    /// W4.6 / D6.3 — mirror of `lunaris_core::audit::audit_events_dropped`,
    /// synced from that atomic on each scrape by
    /// [`sync_audit_drops`]. Not incremented at the drop site: the drop
    /// happens in `lunaris-core`, which carries no metrics dependency and is
    /// reached from every surface (MCP, hook, CLI, HTTP), not only the one
    /// serving `/metrics`.
    pub audit_events_dropped_total: IntCounter,
    /// 0.6.2 P0-3 — `1` when the last `/readyz` probe passed every check,
    /// `0` otherwise. Alert on `lunaris_ready == 0` for longer than one
    /// readiness period: it means the backend PINGs but cannot serve.
    pub ready: IntGauge,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Lazily initialize + return the global [`Metrics`] handle. Idempotent —
/// subsequent calls return the same registry without re-registering.
pub fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| Metrics {
        ingest_total: register_int_counter_vec!(
            "lunaris_ingest_total",
            "Total ingest calls dispatched through POST /v1/ingest",
            &["tenant", "status"]
        )
        .expect("register lunaris_ingest_total"),
        ingest_duration: register_histogram_vec!(
            "lunaris_ingest_duration_seconds",
            "End-to-end ingest latency observed by POST /v1/ingest",
            &["tenant"]
        )
        .expect("register lunaris_ingest_duration_seconds"),
        recall_total: register_int_counter_vec!(
            "lunaris_recall_total",
            "Total recall calls dispatched through POST /v1/recall",
            &["tenant", "mode", "status"]
        )
        .expect("register lunaris_recall_total"),
        recall_duration: register_histogram_vec!(
            "lunaris_recall_duration_seconds",
            "End-to-end recall latency observed by POST /v1/recall",
            &["tenant", "mode"]
        )
        .expect("register lunaris_recall_duration_seconds"),
        forget_total: register_int_counter_vec!(
            "lunaris_forget_total",
            "Total forget calls dispatched through POST /v1/forget",
            &["tenant", "target_kind", "hard"]
        )
        .expect("register lunaris_forget_total"),
        verify_queue_depth: register_int_gauge_vec!(
            "lunaris_verify_queue_depth",
            "Number of pending messages on the verifier queue (polled every 10s)",
            &["topic"]
        )
        .expect("register lunaris_verify_queue_depth"),
        consolidator_queue_depth: register_int_gauge_vec!(
            "lunaris_consolidator_queue_depth",
            "Number of pending messages on the consolidator queue (polled every 10s)",
            &["topic"]
        )
        .expect("register lunaris_consolidator_queue_depth"),
        error_total: register_int_counter_vec!(
            "lunaris_error_total",
            "Total LunarisError occurrences mapped through middleware/error.rs::map_error",
            &["kind"]
        )
        .expect("register lunaris_error_total"),
        eval_score: register_gauge_vec!(
            "lunaris_eval_score",
            "Eval gauntlet score per harness (populated by Plan 05-06 lunaris-evals)",
            &["harness"]
        )
        .expect("register lunaris_eval_score"),
        hotkey_samples: register_int_gauge_vec!(
            "lunaris_hotkey_samples",
            "Sampled hot-key pressure per scope+kind (Moon HOTKEYS: 1-in-64 sampling, \
             multiply by 64 for approx command count; SpaceSaving top-128 ranking, \
             cumulative since Moon start — read as a pressure RANKING, not a rate)",
            &["scope", "kind"]
        )
        .expect("register lunaris_hotkey_samples"),
        http_in_flight: register_int_gauge!(
            "lunaris_http_in_flight",
            "HTTP requests currently being served (incremented on entry to the router, \
             decremented on response OR on drop, so a cancelled request cannot leak)"
        )
        .expect("register lunaris_http_in_flight"),
        http_shed_total: register_int_counter!(
            "lunaris_http_shed_total",
            "Requests rejected by the --http-concurrency limit (503 + Retry-After)"
        )
        .expect("register lunaris_http_shed_total"),
        http_timeout_total: register_int_counter!(
            "lunaris_http_timeout_total",
            "Requests cut off by the --http-timeout-secs budget (408)"
        )
        .expect("register lunaris_http_timeout_total"),
        audit_events_dropped_total: register_int_counter!(
            "lunaris_audit_events_dropped_total",
            "Audit events produced but never delivered to the broker (serialize or publish \
             failure). Fire-and-forget by design — a broker hiccup must not roll back a \
             committed write — so a non-zero value means an audit gap, not a failed request"
        )
        .expect("register lunaris_audit_events_dropped_total"),
        ready: register_int_gauge!(
            "lunaris_ready",
            "1 when the last /readyz probe passed storage PING + write canary + embedder \
             checks, 0 otherwise"
        )
        .expect("register lunaris_ready"),
    })
}

/// Pull `lunaris-core`'s process-wide dropped-audit-event count into the
/// prometheus counter. Call immediately before `prometheus::gather()`.
///
/// The core counter is monotonic and the prometheus one only moves forward, so
/// the sync is the difference between them. `saturating_sub` covers the one
/// case that would otherwise underflow: a test registry reset leaving the
/// exported value above the core value.
pub fn sync_audit_drops() {
    let m = metrics();
    let core = lunaris_core::audit::audit_events_dropped();
    let exported = m.audit_events_dropped_total.get() as u64;
    m.audit_events_dropped_total.inc_by(core.saturating_sub(exported));
}

/// Map a [`lunaris_core::LunarisError`] variant to its bounded `kind` label
/// per the D-25 cardinality cap.
///
/// This used to be a local match ending in `_ => "unknown"`, above a comment
/// instructing the next person to extend it. `LunarisError` is
/// `#[non_exhaustive]`, so that wildcard was mandatory and the compiler could
/// not enforce the instruction — when `Scope` was added, this label silently
/// became "unknown" and the test asserting otherwise was named
/// `error_kind_maps_every_lunaris_error_variant`.
///
/// [`lunaris_core::Subsystem`] moves the classifying match inside the crate
/// that owns the enum, where exhaustiveness is checked. The cardinality cap is
/// now bounded by `Subsystem::ALL.len()` rather than by vigilance.
pub fn error_kind(err: &lunaris_core::LunarisError) -> &'static str {
    err.subsystem().label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::{Encoder, TextEncoder};

    #[test]
    fn metrics_registers_without_panic() {
        let m = metrics();
        // Touch every counter/gauge/histogram to ensure they're queryable
        // (registers a fresh time series per label combo).
        m.ingest_total.with_label_values(&["t", "ok"]).inc();
        m.ingest_duration.with_label_values(&["t"]).observe(0.001);
        m.recall_total.with_label_values(&["t", "semantic", "ok"]).inc();
        m.recall_duration.with_label_values(&["t", "semantic"]).observe(0.001);
        m.forget_total.with_label_values(&["t", "id", "false"]).inc();
        m.verify_queue_depth.with_label_values(&["__lunaris_verify__"]).set(0);
        m.consolidator_queue_depth.with_label_values(&["__lunaris_consolidate__"]).set(0);
        m.error_total.with_label_values(&["storage"]).inc();
        m.eval_score.with_label_values(&["longmemeval"]).set(0.0);
    }

    #[test]
    fn metrics_text_format_contains_all_nine() {
        // Force registry init.
        let _ = metrics();
        // Touch each metric so a series exists and its NAME shows up in the
        // text-format output (a metric with no series isn't emitted by the
        // prometheus crate's TextEncoder until it has at least one observed
        // label combo).
        let m = metrics();
        m.ingest_total.with_label_values(&["t", "ok"]).inc();
        m.ingest_duration.with_label_values(&["t"]).observe(0.001);
        m.recall_total.with_label_values(&["t", "semantic", "ok"]).inc();
        m.recall_duration.with_label_values(&["t", "semantic"]).observe(0.001);
        m.forget_total.with_label_values(&["t", "id", "false"]).inc();
        m.verify_queue_depth.with_label_values(&["__lunaris_verify__"]).set(0);
        m.consolidator_queue_depth.with_label_values(&["__lunaris_consolidate__"]).set(0);
        m.error_total.with_label_values(&["storage"]).inc();
        m.eval_score.with_label_values(&["longmemeval"]).set(0.0);

        let mut buf = Vec::new();
        TextEncoder::new().encode(&prometheus::gather(), &mut buf).expect("encode");
        let out = String::from_utf8(buf).expect("utf8");
        for name in [
            "lunaris_ingest_total",
            "lunaris_ingest_duration_seconds",
            "lunaris_recall_total",
            "lunaris_recall_duration_seconds",
            "lunaris_forget_total",
            "lunaris_verify_queue_depth",
            "lunaris_consolidator_queue_depth",
            "lunaris_error_total",
            "lunaris_eval_score",
        ] {
            assert!(out.contains(name), "metrics text format must contain {name}; got:\n{out}");
        }
    }

    /// W4.6 / D6.3 — the drop counter must actually reach a scraper.
    ///
    /// Counting a drop in `lunaris-core` and never exporting it leaves the
    /// operator exactly where G3 left them. This asserts the sync moves the
    /// exported series to match the core atomic, and that the series name
    /// appears in the text format a Prometheus scraper reads.
    #[tokio::test]
    async fn the_core_drop_count_reaches_the_exported_series() {
        use lunaris_core::Scope;
        use lunaris_core::audit::{
            AuditEvent, ForgetReceiptData, ForgetTargetData, IndexKindData, PublishError,
            Publisher, ScopeSpecData, publish_audit_event,
        };
        use lunaris_core::storage::types::Lsn;

        struct FailingPublisher;
        #[async_trait::async_trait]
        impl Publisher for FailingPublisher {
            async fn publish(
                &self,
                _scope: &Scope,
                _topic: &str,
                _partition: u16,
                _payload: bytes::Bytes,
            ) -> Result<u64, PublishError> {
                Err(PublishError::Backend("broker down".into()))
            }
        }

        let m = metrics();
        sync_audit_drops();
        let exported_before = m.audit_events_dropped_total.get();

        let event = AuditEvent::Forget(ForgetReceiptData {
            target: ForgetTargetData::Scope(ScopeSpecData::BySource("x".into())),
            indices_affected: vec![IndexKindData::Kv],
            rows_written: 1,
            rows_deleted: 0,
            audit_lsn: Lsn { wall_ms: 1, counter: 0 },
            preview: false,
        });
        publish_audit_event(&FailingPublisher, &Scope::new("tenant-a").unwrap(), event)
            .await
            .expect("fire-and-forget must not propagate");

        sync_audit_drops();
        assert!(
            m.audit_events_dropped_total.get() > exported_before,
            "a dropped audit event did not move the exported series ({exported_before} -> {}), \
             so the gap stays invisible to a scraper",
            m.audit_events_dropped_total.get()
        );

        let mut buf = Vec::new();
        TextEncoder::new().encode(&prometheus::gather(), &mut buf).expect("encode");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(
            out.contains("lunaris_audit_events_dropped_total"),
            "the drop counter is missing from the text format a scraper reads"
        );
    }

    #[test]
    fn error_kind_maps_every_lunaris_error_variant() {
        use lunaris_core::{
            ConsolError, ExtractError, LunarisError, RetrieveError, ScopeError, StorageError,
            ValidateError,
        };
        assert_eq!(
            error_kind(&LunarisError::Storage(StorageError::Backend("x".into()))),
            "storage"
        );
        assert_eq!(error_kind(&LunarisError::Validate(ValidateError::Temporal)), "validate");
        assert_eq!(
            error_kind(&LunarisError::Extract(ExtractError::Backend("x".into()))),
            "extract"
        );
        assert_eq!(
            error_kind(&LunarisError::Retrieve(RetrieveError::Backend("x".into()))),
            "retrieve"
        );
        assert_eq!(
            error_kind(&LunarisError::Consolidate(ConsolError::Backend("x".into()))),
            "consolidate"
        );
        // `Scope` is a top-level variant too (added by W4.18 so cookbook code
        // can `?` a `Scope::new`). This test's name claims totality; until now
        // it covered 5 of 6 and the sixth fell through to "unknown".
        assert_eq!(
            error_kind(&LunarisError::Scope(ScopeError::Invalid("bad:scope".into()))),
            "scope"
        );
    }

    /// The list above is hand-written, so it can fall behind the enum exactly
    /// the way it just did. `Subsystem::ALL` cannot: it is generated from the
    /// same macro invocation as `LunarisError::subsystem`, whose match is
    /// exhaustive *inside* the defining crate. Walking it here turns "someone
    /// added a variant and forgot this file" into a failure right here.
    #[test]
    fn error_kind_agrees_with_the_core_subsystem_tag() {
        for sub in lunaris_core::Subsystem::ALL {
            assert_ne!(
                sub.label(),
                "unknown",
                "{sub:?} has no bounded metrics label; extend the D-25 match"
            );
        }
    }
}
