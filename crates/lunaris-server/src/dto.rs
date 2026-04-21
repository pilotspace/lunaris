//! Plan 05-01 — JSON wire DTOs for MemoryProtocol verbs (D-03 + D-05).
//!
//! Every DTO is plain serde + `serde(rename_all = "lowercase")` where the wire
//! contract requires it. Internal types (`ForgetTarget`, `Lsn`) are
//! re-exported as-is so the JSON shape mirrors the Rust shape.

use serde::{Deserialize, Serialize};

use lunaris_core::storage::types::Lsn;

/// `POST /v1/recall` request body. Two retrieval modes per D-05 + PROTO-03.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallRequest {
    /// Free-form query text.
    pub query: String,
    /// Top-k cap; defaults to 10 (matches Phase 2 PROTO-02 default).
    #[serde(default = "default_k")]
    pub k: usize,
    /// Optional RFC-3339 wall-clock timestamp; parsed to `Hlc` by handler.
    pub as_of: Option<String>,
    /// v0 string-DSL filter — passed through to `RetrievalBuilder::filter_str`.
    pub filter: Option<String>,
    /// Retrieval mode (D-05 / PROTO-03).
    #[serde(default)]
    pub mode: RetrievalMode,
}

fn default_k() -> usize {
    10
}

/// Two retrieval modes per CONTEXT.md D-05 + PROTO-03.
///
/// `Semantic` runs the Phase 2 hot path (Vector + Keyword(BM25) + RRF +
/// bge-rerank). `Graph` runs Phase 3 `Graph::anchored` and is gated on
/// `StorageCapabilities::graph_native || lunaris.graph_pipeline().is_enabled()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RetrievalMode {
    #[default]
    Semantic,
    Graph,
}

/// `POST /v1/ingest` response body — D-03 verbatim shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResponse {
    pub lsn: Lsn,
    /// `true` when verifier-queue depth exceeds the warn threshold (B-9
    /// surface from Plan 04-04). Best-effort; backends without `queue_depth`
    /// support always report `false`.
    pub queue_lag_warn: bool,
}

/// `POST /v1/forget` request body.
///
/// D-21 two-step rail: when `hard=true`, the caller MUST first run
/// `dry_run=true`, then re-issue with the previous `audit_lsn` as the
/// `confirmation_token` (encoded as `"<wall_ms>.<counter>"`). Missing token →
/// 428 Precondition Required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetRequestDto {
    pub target: lunaris::ForgetTarget,
    #[serde(default)]
    pub hard: bool,
    #[serde(default)]
    pub dry_run: bool,
    /// Token from a prior `dry_run` receipt's `audit_lsn`.
    pub confirmation_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_request_default_mode_is_semantic() {
        let body = serde_json::json!({"query":"hi","k":5});
        let req: RecallRequest = serde_json::from_value(body).expect("parse");
        assert_eq!(req.mode, RetrievalMode::Semantic);
        assert_eq!(req.k, 5);
    }

    #[test]
    fn recall_request_default_k_is_10() {
        let body = serde_json::json!({"query":"hi"});
        let req: RecallRequest = serde_json::from_value(body).expect("parse");
        assert_eq!(req.k, 10);
    }

    #[test]
    fn retrieval_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&RetrievalMode::Semantic).unwrap(),
            "\"semantic\""
        );
        assert_eq!(
            serde_json::to_string(&RetrievalMode::Graph).unwrap(),
            "\"graph\""
        );
    }
}
