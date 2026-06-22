//! Plan 05-01 — `POST /v1/recall` (PROTO-02 + PROTO-03 + D-04 + D-05).
//!
//! Two response shapes per D-04:
//! - `Accept: application/json` (default) → `Vec<Hit>` JSON body in one response.
//! - `Accept: text/event-stream` → SSE stream of `event: hit\ndata: <Hit>\n\n`
//!   per hit, terminated by `event: done\ndata: {}\n\n`.
//!
//! Two retrieval modes per D-05:
//! - `mode: "semantic"` (default) → Phase 2 hot path (Vector + Keyword + RRF + bge-rerank).
//! - `mode: "graph"` → Phase 3 `Graph::anchored` operator, gated on
//!   `capabilities().graph_native || lunaris.graph_pipeline().is_enabled()`.

use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use futures::stream::{self, Stream};

use lunaris_core::Hlc;
use lunaris_core::storage::types::Filter;
use lunaris_retrieve::{DEFAULT_GRAPH_HOPS, EntityId, Graph, Hit, Query, Vector};

use crate::dto::{
    RecallRequest, RetrievalMode, categories_filter, compose_request_scope, validate_categories,
};
use crate::metrics::metrics;
use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::routes::ingest::level_reject;
use crate::state::AppState;

pub async fn recall_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    headers: HeaderMap,
    Json(req): Json<RecallRequest>,
) -> Response {
    // RFC 0001 Wave 1E: use the JWT-bound scope for metrics labels and for
    // the scoped degraded-check instead of `Scope::dev()`.
    let scope_str = claims.scope.as_str();
    let mode_label = match req.mode {
        RetrievalMode::Semantic => "semantic",
        RetrievalMode::Graph => "graph",
    };

    // Plan 05-05 OPS-06 — duration timer started up-front so even early-exit
    // paths (graph-mode unavailable, invalid filter) contribute to the
    // histogram observation when the metric increments below.
    let timer = metrics().recall_duration.with_label_values(&[scope_str, mode_label]).start_timer();

    // D-05 + PROTO-03: gate "graph" mode on backend capability + runtime
    // toggle. v0 returns 501 Not Implemented when neither is satisfied —
    // future work wires the Graph::anchored operator into compose_query.
    if req.mode == RetrievalMode::Graph {
        let caps = state.lunaris.storage().capabilities();
        let graph_runtime_on = state.lunaris.graph_pipeline().is_enabled();
        if !caps.graph_native && !graph_runtime_on {
            timer.observe_duration();
            metrics().recall_total.with_label_values(&[scope_str, mode_label, "error"]).inc();
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(serde_json::json!({
                    "error": "graph_mode_unavailable",
                    "message": "graph mode requires capabilities().graph_native or GraphPipeline::enable()",
                })),
            )
                .into_response();
        }
    }

    // Build the typed Query from the wire DTO up-front so request-level
    // validation surfaces as 400 BEFORE we touch the storage layer.
    let query = match build_query(&req) {
        Ok(q) => q,
        Err(msg) => {
            timer.observe_duration();
            metrics().recall_total.with_label_values(&[scope_str, mode_label, "error"]).inc();
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_request",
                    "message": msg,
                })),
            )
                .into_response();
        }
    };

    // multi-level-memory-categories: compose the SAME partition a matching
    // level-tagged ingest wrote, and validate categories. Reject with 400
    // (the contract's error codes) before touching storage.
    let err_inc =
        || metrics().recall_total.with_label_values(&[scope_str, mode_label, "error"]).inc();
    let scope = match compose_request_scope(
        &claims.scope,
        req.user_id.as_deref(),
        req.agent_id.as_deref(),
        req.session_id.as_deref(),
    ) {
        Ok(s) => s,
        Err(e) => {
            timer.observe_duration();
            err_inc();
            return level_reject(e);
        }
    };
    if let Err(e) = validate_categories(&req.categories) {
        timer.observe_duration();
        err_inc();
        return level_reject(e);
    }

    // RFC 0001 Wave 1E: use engine.scoped(scope) to get the scope-aware
    // RetrievalBuilder, now bound to the COMPOSED scope (JWT base is its
    // prefix). ScopedLunaris::dsl() returns a RetrievalBuilder pre-seeded with
    // the engine's storage/embedder/keyword Arcs.
    let scoped = state.lunaris.scoped(scope.clone());
    let mut builder = scoped.dsl();

    // Parse the optional string-DSL filter — surface parse failures as 400
    // (the body was valid JSON but the filter DSL is invalid).
    let string_filter = match &req.filter {
        Some(s) => match lunaris_retrieve::filter_str(s) {
            Ok(f) => Some(f),
            Err(e) => {
                timer.observe_duration();
                err_inc();
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_filter",
                        "message": format!("filter DSL parse error: {e}"),
                    })),
                )
                    .into_response();
            }
        },
        None => None,
    };
    // AND-combine the string-DSL filter with the categories filter.
    let combined = match (string_filter, categories_filter(&req.categories)) {
        (Some(a), Some(b)) => Some(Filter::And(vec![a, b])),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    if let Some(f) = combined {
        builder = builder.filter(f);
    }

    // Plan 07-01 — compose Graph::anchored into the root when mode=graph and
    // the 501 gate passed. Empty-entity fallback marks each hit degraded=true
    // per ROADMAP Phase 7 SC #2.
    let mut fallback_degraded = false;
    if req.mode == RetrievalMode::Graph {
        let entities = extract_query_entities(&req.query);
        if entities.is_empty() {
            fallback_degraded = true;
        } else {
            // Wave 4 piece A: Graph::anchored takes Vec<(EntityId, f32)>.
            // The RETRIEVE-13 planner stub does NOT yet emit per-entity
            // confidence; pass 1.0 (full confidence) for every extracted
            // entity. When the planner learns to emit confidence (tracked
            // in v0.4 known-debt), this becomes a direct passthrough.
            let seeds: Vec<(_, f32)> = entities.into_iter().map(|e| (e, 1.0_f32)).collect();
            builder = builder.with_root(
                Vector::new("chunks", 30)
                    .and(Graph::anchored(seeds, DEFAULT_GRAPH_HOPS))
                    .fuse_rrf(60),
            );
        }
    }

    let want_sse = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false);

    let result = builder.execute(query).await;
    timer.observe_duration();
    let status = if result.is_ok() { "ok" } else { "error" };
    metrics().recall_total.with_label_values(&[scope_str, mode_label, status]).inc();

    // Plan 07-01 — mark each hit degraded=true when graph-mode fell back.
    let result = result.map(|mut hits| {
        if fallback_degraded {
            for h in hits.iter_mut() {
                h.degraded = true;
            }
        }
        hits
    });

    if want_sse {
        match result {
            Ok(hits) => sse_response(hits),
            Err(e) => map_error(e),
        }
    } else {
        match result {
            Ok(hits) => (StatusCode::OK, Json(hits)).into_response(),
            Err(e) => map_error(e),
        }
    }
}

/// v0 entity extraction for HTTP mode=graph — mirrors
/// `lunaris_retrieve::planner::has_entity_like_capitalized_token` inline
/// (Phase 7 risk register: no new extractor, no new query-planner strategy).
/// Walks whitespace tokens; every capitalized non-first token mints an
/// `EntityId::from_name_and_type(token, "")`. Empty `entity_type` reflects
/// the v0 "no type-tagging at HTTP" constraint — the caller surfaces the
/// empty-hit case via `degraded=true`.
fn extract_query_entities(text: &str) -> Vec<EntityId> {
    text.split_whitespace()
        .enumerate()
        .filter_map(|(i, tok)| {
            if i == 0 {
                return None;
            }
            // Strip leading non-alphanumeric (matches planner.rs token logic).
            let start = tok.find(|c: char| c.is_ascii_alphanumeric())?;
            let clean = &tok[start..];
            let first = clean.chars().next()?;
            if first.is_ascii_uppercase() {
                // Strip trailing punctuation for cleaner names.
                let end = clean
                    .rfind(|c: char| c.is_ascii_alphanumeric())
                    .map(|idx| idx + 1)
                    .unwrap_or(clean.len());
                Some(EntityId::from_name_and_type(&clean[..end], ""))
            } else {
                None
            }
        })
        .collect()
}

/// Build the typed `Query` from the wire DTO.
fn build_query(req: &RecallRequest) -> Result<Query, &'static str> {
    let mut q = Query::text(&req.query);
    if req.k > 0 {
        q.k = req.k;
    }
    if let Some(ts) = &req.as_of {
        match parse_rfc3339_to_hlc(ts) {
            Ok(hlc) => q.as_of = Some(hlc),
            Err(_) => return Err("as_of must be RFC-3339"),
        }
    }
    Ok(q)
}

fn parse_rfc3339_to_hlc(s: &str) -> Result<Hlc, ()> {
    use chrono::DateTime;
    let dt = DateTime::parse_from_rfc3339(s).map_err(|_| ())?;
    let wall_ms = u64::try_from(dt.timestamp_millis()).map_err(|_| ())?;
    Ok(Hlc { wall_ms, counter: 0, node_id: 0 })
}

/// Build the SSE response stream — one `event: hit` per Hit, terminal
/// `event: done` envelope.
fn sse_response(hits: Vec<Hit>) -> Response {
    let events: Vec<Result<Event, Infallible>> = hits
        .into_iter()
        .map(|hit| {
            let payload = serde_json::to_string(&hit).unwrap_or_else(|_| "{}".to_string());
            Ok(Event::default().event("hit").data(payload))
        })
        .collect();
    let done =
        stream::once(async { Ok::<Event, Infallible>(Event::default().event("done").data("{}")) });
    let stream: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(stream::iter(events).chain(done));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rfc3339_round_trips() {
        let h = parse_rfc3339_to_hlc("2025-06-01T00:00:00Z").expect("parse");
        assert!(h.wall_ms > 0);
        assert_eq!(h.counter, 0);
    }

    #[test]
    fn parse_rfc3339_rejects_garbage() {
        assert!(parse_rfc3339_to_hlc("not a date").is_err());
    }

    #[test]
    fn build_query_uses_default_k_when_zero() {
        let req = RecallRequest {
            query: "hi".into(),
            k: 0,
            as_of: None,
            filter: None,
            mode: RetrievalMode::Semantic,
            user_id: None,
            agent_id: None,
            session_id: None,
            categories: Vec::new(),
        };
        let q = build_query(&req).expect("ok");
        // build_query keeps Query::text default k (30) when req.k == 0.
        assert_eq!(q.k, 30);
    }

    // ---- Plan 07-01 extract_query_entities unit tests ---------------------

    #[test]
    fn extract_query_entities_matches_rust_sdk_shape() {
        // "what did Alice do at Acme last April?" → Alice + Acme + April.
        let ents = extract_query_entities("what did Alice do at Acme last April?");
        assert_eq!(ents.len(), 3, "Alice + Acme + April");
        let alice = EntityId::from_name_and_type("Alice", "");
        assert!(ents.contains(&alice), "expected Alice EntityId in {ents:?}",);
    }

    #[test]
    fn extract_query_entities_empty_on_lowercase_query() {
        assert!(extract_query_entities("show me everything lowercase").is_empty());
    }

    #[test]
    fn extract_query_entities_skips_sentence_initial_capitalization() {
        // "What" is sentence-initial — NOT an entity (matches planner.rs behavior).
        assert!(extract_query_entities("What is going on").is_empty());
    }
}
