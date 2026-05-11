//! Plan 05-01 — JSON wire DTOs for MemoryProtocol verbs (D-03 + D-05).
//!
//! Every DTO is plain serde + `serde(rename_all = "lowercase")` where the wire
//! contract requires it. Internal types (`ForgetTarget`, `Lsn`) are
//! re-exported as-is so the JSON shape mirrors the Rust shape.
//!
//! ## RFC 0001 Wave 1E — `IngestBody` replaces bare `Episode` on the wire
//!
//! The `POST /v1/ingest` endpoint previously accepted a raw `Episode` (which
//! carries `pub scope: Scope`) — this let a client set an arbitrary scope and
//! override the JWT-bound scope. `IngestBody` removes the `scope` field from
//! the wire entirely; the handler stamps the JWT-bound scope onto the episode
//! before persisting. Unknown fields are rejected (`deny_unknown_fields`) to
//! prevent clients from passing `scope` through a different key.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use lunaris_core::storage::types::Lsn;

/// `POST /v1/ingest` request body — RFC 0001 Wave 1E shape.
///
/// The `scope` field is intentionally absent: the JWT-bound scope (from
/// `AuthClaims.scope`) is the authoritative partition key. Clients MUST NOT
/// supply `scope` — the field is stripped at the HTTP boundary. Unknown fields
/// are rejected entirely (`deny_unknown_fields`) so `"scope"` passed by a
/// client is an immediate 422 Unprocessable Entity, not a silent override.
///
/// The `id` field is optional: when absent, the handler generates a fresh
/// `Ulid`. When present, callers may supply a deterministic id for idempotent
/// ingest (e.g., replay / migration tooling).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestBody {
    /// Optional client-supplied id. When absent a fresh ULID is generated.
    pub id: Option<Ulid>,
    /// Observation source (agent id, tool name, document path, …).
    pub source: String,
    /// Free-form text content of the observation.
    pub content: String,
    /// Optional valid-time anchor (RFC-3339). When absent, wall-clock now.
    pub t_ref: Option<DateTime<Utc>>,
    /// Additional metadata key-value pairs. MUST NOT contain a `"tenant"`
    /// or `"scope"` key — those are rejected at deserialization time because
    /// `deny_unknown_fields` applies to the top-level struct. Nested metadata
    /// values are passed through as-is.
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// `POST /v1/recall` request body. Two retrieval modes per D-05 + PROTO-03.
///
/// P-1 (v0.2 release-gate review): `deny_unknown_fields` closes the
/// `scope` / `tenant` smuggling vector at the wire boundary, matching
/// `IngestBody`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

    // ---- IngestBody RFC 0001 Wave 1E tests --------------------------------

    #[test]
    fn ingest_body_rejects_scope_field() {
        // A client that includes "scope" in the JSON body MUST get a
        // deserialization error — the field is not part of the wire contract.
        let body = serde_json::json!({
            "source": "agent-42",
            "content": "hello",
            "scope": "evil"          // NOT allowed — deny_unknown_fields
        });
        let result: Result<IngestBody, _> = serde_json::from_value(body);
        assert!(result.is_err(), "scope field must be rejected by deny_unknown_fields");
    }

    #[test]
    fn ingest_body_rejects_tenant_field() {
        // Legacy v0.1 clients may send metadata.tenant at the top level
        // (or as a plain "tenant" key). Both must be rejected.
        let body = serde_json::json!({
            "source": "agent-42",
            "content": "hello",
            "tenant": "evil"         // NOT allowed — deny_unknown_fields
        });
        let result: Result<IngestBody, _> = serde_json::from_value(body);
        assert!(result.is_err(), "tenant field must be rejected by deny_unknown_fields");
    }

    #[test]
    fn ingest_body_accepts_valid_fields() {
        let body = serde_json::json!({
            "source": "agent-42",
            "content": "hello world",
            "metadata": { "key": "value" }
        });
        let parsed: IngestBody = serde_json::from_value(body).expect("valid IngestBody");
        assert_eq!(parsed.source, "agent-42");
        assert_eq!(parsed.content, "hello world");
        assert_eq!(parsed.metadata.get("key").unwrap(), "value");
        assert!(parsed.id.is_none());
    }

    #[test]
    fn ingest_body_metadata_may_contain_custom_keys() {
        // Arbitrary metadata keys (not "scope" / "tenant") are allowed.
        let body = serde_json::json!({
            "source": "s",
            "content": "c",
            "metadata": { "custom_key": "custom_val", "nested": { "x": 1 } }
        });
        let parsed: IngestBody = serde_json::from_value(body).expect("valid IngestBody");
        assert!(parsed.metadata.contains_key("custom_key"));
        assert!(parsed.metadata.contains_key("nested"));
    }

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

    // ---- P-1 (v0.2 release-gate) — deny_unknown_fields parity for the
    //       two remaining DTOs. Mirrors `ingest_body_rejects_scope_field` /
    //       `ingest_body_rejects_tenant_field`. ---------------------------

    #[test]
    fn recall_request_rejects_scope_field() {
        let body = serde_json::json!({"query":"hi","scope":"evil"});
        let result: Result<RecallRequest, _> = serde_json::from_value(body);
        assert!(result.is_err(), "scope field MUST be rejected on RecallRequest");
    }

    #[test]
    fn recall_request_rejects_tenant_field() {
        let body = serde_json::json!({"query":"hi","tenant":"evil"});
        let result: Result<RecallRequest, _> = serde_json::from_value(body);
        assert!(result.is_err(), "tenant field MUST be rejected on RecallRequest");
    }

    #[test]
    fn forget_request_rejects_scope_field() {
        // Use a minimal ForgetTarget that round-trips through serde.
        let body = serde_json::json!({
            "target": {"Id": "01HZZZZZZZZZZZZZZZZZZZZZZZ"},
            "scope": "evil"
        });
        let result: Result<ForgetRequestDto, _> = serde_json::from_value(body);
        assert!(result.is_err(), "scope field MUST be rejected on ForgetRequestDto");
    }

    #[test]
    fn forget_request_rejects_tenant_field() {
        let body = serde_json::json!({
            "target": {"Id": "01HZZZZZZZZZZZZZZZZZZZZZZZ"},
            "tenant": "evil"
        });
        let result: Result<ForgetRequestDto, _> = serde_json::from_value(body);
        assert!(result.is_err(), "tenant field MUST be rejected on ForgetRequestDto");
    }

    #[test]
    fn retrieval_mode_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&RetrievalMode::Semantic).unwrap(), "\"semantic\"");
        assert_eq!(serde_json::to_string(&RetrievalMode::Graph).unwrap(), "\"graph\"");
    }
}
