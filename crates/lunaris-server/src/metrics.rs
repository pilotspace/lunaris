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
    GaugeVec, HistogramVec, IntCounterVec, IntGaugeVec, register_gauge_vec, register_histogram_vec,
    register_int_counter_vec, register_int_gauge_vec,
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
    })
}

/// Map a [`lunaris_core::LunarisError`] variant to its bounded `kind` label
/// per the D-25 cardinality cap. New variants in the future MUST extend this
/// match before the cap of ~10 distinct values is exceeded.
pub fn error_kind(err: &lunaris_core::LunarisError) -> &'static str {
    use lunaris_core::LunarisError;
    match err {
        LunarisError::Storage(_) => "storage",
        LunarisError::Validate(_) => "validate",
        LunarisError::Extract(_) => "extract",
        LunarisError::Retrieve(_) => "retrieve",
        LunarisError::Consolidate(_) => "consolidate",
        _ => "unknown",
    }
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

    #[test]
    fn error_kind_maps_every_lunaris_error_variant() {
        use lunaris_core::{
            ConsolError, ExtractError, LunarisError, RetrieveError, StorageError, ValidateError,
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
    }
}
