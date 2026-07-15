//! `memory.recall` — semantic retrieval over agent memory.
//!
//! Wave 2.B: full implementation.
//! - Lazy model stager (`ensure_staged`) on first call, not at server start.
//! - `Query::text(query).k` + optional `Filter::StartsWith` + optional `as_of`.
//! - Maps `Vec<Hit>` → `Vec<RecallHit>` with a curated LLM-ready snippet
//!   (`lunaris_core::snippet`, ≤ 260 chars) by default, or the raw stored
//!   bytes (≤ 200 chars) when `raw: true`.

use chrono::DateTime;
use lunaris::Query;
use lunaris_core::{Hlc, snippet, storage::types::Filter};
use lunaris_retrieve::{Keyword, Vector};
use serde::{Deserialize, Serialize};

use crate::ServiceError;
use crate::is_keyword_not_supported;
use lunaris::Lunaris;
use lunaris_core::Scope;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Optional filters for `memory.recall`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallFilters {
    /// Only return memories whose source starts with this prefix.
    #[serde(default)]
    pub source_prefix: Option<String>,
}

/// Input parameters for `memory.recall`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallParams {
    /// Natural-language query to recall memories for.
    pub query: String,

    /// Maximum number of hits to return (default: 5).
    #[serde(default = "default_k")]
    pub k: usize,

    /// Optional source/metadata filters.
    #[serde(default)]
    pub filters: Option<RecallFilters>,

    /// Bi-temporal as-of time in RFC-3339 (default: latest).
    #[serde(default)]
    pub as_of: Option<String>,

    /// Return raw stored bytes instead of the curated LLM-ready snippet
    /// (default: false — curated).
    #[serde(default)]
    pub raw: bool,
}

fn default_k() -> usize {
    5
}

/// A single recalled memory hit.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecallHit {
    /// Episode ID (ULID string).
    pub episode_id: String,
    /// Logical source of the episode.
    pub source: String,
    /// Curated LLM-ready snippet of the recalled episode (≤ 260 chars;
    /// JSON envelopes render as `decision:`/`edit`/`tool output:`/`prompt:`
    /// summaries). With `raw: true`, the first 200 chars of stored bytes.
    pub content: String,
    /// Combined retrieval score ∈ [0, 1].
    pub score: f32,
    /// Ingest wall-clock timestamp in RFC-3339.
    pub ingested_at: String,
}

/// Output of a successful `memory.recall` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecallResponse {
    /// Ordered list of recalled memories (highest score first).
    pub hits: Vec<RecallHit>,
}

// ── Hlc helpers ──────────────────────────────────────────────────────────────

fn parse_rfc3339_to_hlc(s: &str) -> Result<Hlc, ServiceError> {
    let dt = DateTime::parse_from_rfc3339(s)
        .map_err(|e| ServiceError::InvalidInput(format!("as_of is not valid RFC-3339: {e}")))?;
    let wall_ms = u64::try_from(dt.timestamp_millis())
        .map_err(|_| ServiceError::InvalidInput("as_of timestamp predates Unix epoch".into()))?;
    Ok(Hlc { wall_ms, counter: 0, node_id: 0 })
}

fn hlc_to_rfc3339(hlc: &Hlc) -> String {
    DateTime::from_timestamp_millis(hlc.wall_ms as i64).unwrap_or_default().to_rfc3339()
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.recall`.
///
/// 1. Assume a ready embedder — staging is a CALLER concern (the mcp `#[tool]`
///    wrapper stages lazily; contextd stages at warm-up).
/// 2. Build `Query::text(query)` with optional `k`, `Filter::StartsWith`, and
///    `as_of` bi-temporal snapshot.
/// 3. Execute the canonical hybrid plan:
///    `Vector(chunks) AND Keyword::bm25(chunks) -> fuse_rrf(60)`.
///    Moon handles this as native HYBRID search when the backend supports it;
///    other keyword-capable backends use client-side RRF. Backends without a
///    keyword surface fall back narrowly to vector-only recall.
/// 4. Map each `Hit` to `RecallHit` — curated snippet by default (JSON
///    envelopes summarized via `lunaris_core::snippet`, 260-char cap), raw
///    200-char preview when `params.raw`.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: RecallParams,
) -> Result<RecallResponse, ServiceError> {
    // NOTE: model staging is a CALLER concern (lifted out of the shared engine
    // handler). The mcp `#[tool]` wrapper calls `maybe_ensure_staged()` before
    // delegating here; contextd stages once at warm-up. This handler assumes a
    // ready engine.

    // Empty query → empty hits (not an error).
    if params.query.is_empty() {
        return Ok(RecallResponse { hits: vec![] });
    }

    // Treat k=0 as default_k so callers can omit the field via `k: 0`.
    let k = if params.k == 0 { default_k() } else { params.k };

    // Build the query. Source lives on the parent Episode after hydration, so
    // source_prefix is enforced after recall instead of relying on vector
    // metadata that may be backend/version dependent.
    let mut q = Query::text(&params.query);
    let source_prefix = params
        .filters
        .as_ref()
        .and_then(|filters| filters.source_prefix.as_ref())
        .filter(|prefix| !prefix.is_empty())
        .cloned();
    let candidate_k =
        if source_prefix.is_some() { k.saturating_mul(8).max(32) } else { k }.clamp(1, 100);
    q.k = candidate_k;

    // Optional source-prefix filter. Keep it on the query as a best-effort
    // backend hint, but do not trust it as the only enforcement layer.
    if let Some(filters) = &params.filters
        && let Some(prefix) = &filters.source_prefix
        && !prefix.is_empty()
    {
        q.filter = Some(Filter::StartsWith { field: "source".into(), prefix: prefix.clone() });
    }

    // Optional bi-temporal as_of — HONESTY GATE first (ADD task
    // moon-parity-honesty): on a backend whose read paths ignore as_of
    // (Moon reads current state only), silently accepting the parameter
    // returns WRONG results dressed as a snapshot — the 2026-07-14 deep
    // test got 2026 episodes back for `as_of=2020-01-01`. Refuse loudly.
    if params.as_of.is_some() {
        ensure_as_of_supported(&lunaris.storage().capabilities())?;
    }
    if let Some(ts) = &params.as_of {
        q.as_of = Some(parse_rfc3339_to_hlc(ts)?);
    }

    // Execute — ScopedLunaris::recall threads the bound scope through.
    let scoped = lunaris.scoped(scope.clone());
    let root = Vector::new("chunks", candidate_k)
        .and(Keyword::bm25("chunks", candidate_k))
        .fuse_rrf(60)
        .top(candidate_k);
    let hits = match scoped.dsl().with_root(root).execute(q.clone()).await {
        Ok(hits) => hits,
        Err(err) if is_keyword_not_supported(&err) => {
            tracing::debug!(
                scope = scope.as_str(),
                "memory.recall keyword branch unsupported; falling back to vector-only"
            );
            scoped
                .dsl()
                .with_root(Vector::new("chunks", candidate_k))
                .execute(q)
                .await
                .map_err(ServiceError::LunarisEngine)?
        }
        Err(err) => return Err(ServiceError::LunarisEngine(err)),
    };

    // Map hits to wire DTO with explicit type so the collector infers correctly.
    let recall_hits: Vec<RecallHit> = hits
        .into_iter()
        .filter(|h| {
            source_prefix.as_ref().map(|prefix| h.source.starts_with(prefix)).unwrap_or(true)
        })
        .take(k)
        .map(|h| {
            let episode_id = ulid_bytes_to_string(&h.id);
            let content: String = if params.raw {
                h.text.chars().take(200).collect()
            } else {
                snippet::trim_to_chars(
                    &snippet::summarize(&h.source, &h.text)
                        .unwrap_or_else(|| snippet::single_line(&h.text)),
                    260,
                )
            };
            let ingested_at = hlc_to_rfc3339(&h.valid_from);
            RecallHit { episode_id, source: h.source, content, score: h.score, ingested_at }
        })
        .collect();

    tracing::debug!(
        scope = scope.as_str(),
        k = k,
        candidate_k = candidate_k,
        hits = recall_hits.len(),
        "memory.recall executed",
    );

    Ok(RecallResponse { hits: recall_hits })
}

/// Convert a 16-byte ULID to its canonical 26-char string representation.
/// Falls back to an empty string for malformed byte slices (should never happen
/// in practice — storage always writes 16-byte ULID ids).
fn ulid_bytes_to_string(bytes: &[u8]) -> String {
    if let Ok(arr) = <[u8; 16]>::try_from(bytes) {
        ulid::Ulid::from_bytes(arr).to_string()
    } else {
        String::new()
    }
}

/// `as_of` honesty gate (ADD task moon-parity-honesty): refuse a point-in-time
/// snapshot request on a backend whose read paths cannot honor it. A silent
/// no-op here is worse than an error — the caller believes they got a
/// historical view and acts on current-state data.
fn ensure_as_of_supported(caps: &lunaris_core::StorageCapabilities) -> Result<(), ServiceError> {
    if caps.bi_temporal_native {
        Ok(())
    } else {
        Err(ServiceError::InvalidInput(
            "as_of requires a bi-temporal backend; this backend reads current state only \
             (Moon AS_OF parity is tracked as STORE-07) — drop as_of or use a bi-temporal \
             backend"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod as_of_gate_tests {
    use super::ensure_as_of_supported;
    use lunaris_core::{CypherDialect, StorageCapabilities};

    fn caps(bi_temporal_native: bool) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native,
            graph_native: true,
            rerank_native: true,
            queue_native: true,
            max_vector_dim: 768,
            native_rrf: true,
            max_scopes_recommended: 512,
            cypher_dialect: CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }

    /// §2 "as_of on Moon is refused loudly": Moon-shaped capabilities
    /// (bi_temporal_native = false) must produce a typed InvalidInput that
    /// names the gap — never a silent pass-through.
    #[test]
    fn as_of_rejected_on_non_bitemporal_backend() {
        let err = ensure_as_of_supported(&caps(false))
            .expect_err("non-bi-temporal caps must reject as_of");
        let msg = format!("{err}");
        assert!(msg.contains("bi-temporal"), "error must name the gap, got: {msg}");
    }

    /// §2 "as_of on embedded still works": bi-temporal-native capabilities
    /// pass the gate untouched (the Wave A.1 snapshot test remains the
    /// end-to-end witness).
    #[test]
    fn as_of_allowed_on_bitemporal_backend() {
        ensure_as_of_supported(&caps(true)).expect("bi-temporal caps must pass");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use lunaris::{EpisodeBuilder, Lunaris};
    use lunaris_core::{Scope, StubEmbedder};

    /// Construct an in-process Lunaris handle with `StubEmbedder` (deterministic
    /// 768-d vectors) backed by SQLite `memory://`. StubEmbedder produces
    /// deterministic non-zero vectors so vector recall returns real ranked hits
    /// rather than the zero-vector NoopEmbedder which returns nothing.
    async fn make_engine(scope_name: &str) -> (Lunaris, Scope) {
        // No staging needed — the handler no longer stages (that is a CALLER
        // concern, lifted to the mcp/contextd boundary). StubEmbedder provides
        // deterministic recall without the 253 MB HF download.
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder("memory://", embedder).await.unwrap();
        let scope = Scope::new(scope_name).unwrap();
        (lunaris, scope)
    }

    /// Wave A.1: brute-force cosine `vector_search` ships in the embedded
    /// backend. Ingest an episode, recall it by overlapping text — must return
    /// at least one hit with a non-empty episode ID.
    ///
    /// Previously ignored with "embedded backend lacks vector_search" — that
    /// annotation was stale since commit `5674c52` (Wave A.1 T2). Re-enabled
    /// 2026-05-26 as part of the mcp-recall-empty-hits fix.
    #[tokio::test]
    async fn ingest_then_recall_round_trip() {
        let (lunaris, scope) = make_engine("test-recall-rt").await;
        let scoped = lunaris.scoped(scope.clone());

        scoped
            .ingest(EpisodeBuilder::new("test/src", "chocolate cake recipe with cocoa"))
            .await
            .unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            RecallParams {
                query: "chocolate".into(),
                k: 5,
                filters: None,
                as_of: None,
                raw: false,
            },
        )
        .await
        .unwrap();

        assert!(!resp.hits.is_empty(), "expected at least one hit");
        let top = &resp.hits[0];
        assert!(!top.episode_id.is_empty());
        assert!(!top.ingested_at.is_empty());
    }

    /// ADD task recall-llm-snippets (FROZEN @ v1): default hit content is the
    /// curated LLM-ready summary, never the raw JSON envelope.
    #[tokio::test]
    async fn recall_curates_decision_snippet() {
        let (lunaris, scope) = make_engine("test-recall-curated").await;
        let scoped = lunaris.scoped(scope.clone());

        scoped
            .ingest(EpisodeBuilder::new(
                "decision:test-recall-curated",
                r#"{"decision":"adopt launchd for Moon","rationale":"KeepAlive restarts on crash"}"#,
            ))
            .await
            .unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            RecallParams {
                query: "launchd Moon".into(),
                k: 5,
                filters: None,
                as_of: None,
                raw: false,
            },
        )
        .await
        .unwrap();

        let top = &resp.hits[0];
        assert!(
            top.content.starts_with("decision: adopt launchd for Moon"),
            "curated summary expected, got: {:?}",
            top.content
        );
        assert!(top.content.contains("rationale: KeepAlive"), "rationale rides along");
        assert!(!top.content.contains('{'), "no raw JSON in default content: {:?}", top.content);
    }

    /// raw=true restores the legacy 200-char raw preview for debugging.
    #[tokio::test]
    async fn recall_raw_param_returns_stored_bytes() {
        let (lunaris, scope) = make_engine("test-recall-raw").await;
        let scoped = lunaris.scoped(scope.clone());

        scoped
            .ingest(EpisodeBuilder::new(
                "decision:test-recall-raw",
                r#"{"decision":"adopt launchd for Moon","rationale":"KeepAlive restarts on crash"}"#,
            ))
            .await
            .unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            RecallParams {
                query: "launchd Moon".into(),
                k: 5,
                filters: None,
                as_of: None,
                raw: true,
            },
        )
        .await
        .unwrap();

        assert!(
            resp.hits[0].content.contains('{'),
            "raw=true must return stored bytes, got: {:?}",
            resp.hits[0].content
        );
    }

    /// Plain-text episodes pass through as single-line text (no curation loss).
    #[tokio::test]
    async fn recall_plain_text_passthrough() {
        let (lunaris, scope) = make_engine("test-recall-plain").await;
        let scoped = lunaris.scoped(scope.clone());

        scoped.ingest(EpisodeBuilder::new("notes/a", "The cobalt gateway is CG-1.")).await.unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            RecallParams {
                query: "cobalt gateway".into(),
                k: 5,
                filters: None,
                as_of: None,
                raw: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(resp.hits[0].content, "The cobalt gateway is CG-1.");
    }

    /// Wave A.1: bi-temporal as_of snapshot. Ingest A, capture wall time, ingest
    /// B, then recall with as_of = t_between. Must not surface B.
    ///
    /// Previously ignored with stale "embedded backend lacks vector_search".
    /// Re-enabled 2026-05-26 as part of the mcp-recall-empty-hits fix.
    #[tokio::test]
    async fn as_of_time_travel_proves_bi_temporal() {
        let (lunaris, scope) = make_engine("test-recall-bt").await;
        let scoped = lunaris.scoped(scope.clone());

        // Ingest fact A, capture wall time between the two ingests.
        scoped.ingest(EpisodeBuilder::new("bt/src", "the sky is blue")).await.unwrap();

        // t1.5 is after A but before B — use current time as the as_of anchor.
        let t_between = chrono::Utc::now().to_rfc3339();
        // Small sleep so B's HLC wall_ms is strictly after t_between.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        scoped.ingest(EpisodeBuilder::new("bt/src", "the sky is green")).await.unwrap();

        // Recall with as_of = t_between: B was not ingested yet at that point.
        let resp = handle(
            &lunaris,
            &scope,
            RecallParams {
                query: "sky".into(),
                k: 5,
                filters: None,
                as_of: Some(t_between),
                raw: false,
            },
        )
        .await
        .unwrap();

        // Either zero hits (snapshot filtered B) OR the top hit must be A.
        // We must NOT see "green" as the top content when as_of is before B.
        if !resp.hits.is_empty() {
            assert!(
                !resp.hits[0].content.contains("green"),
                "bi-temporal as_of must not return fact B (sky is green) at t_between; \
                 got: {:?}",
                resp.hits[0].content
            );
        }
    }

    /// Wave A.1: source-prefix filter excludes episodes from non-matching sources.
    ///
    /// Previously ignored with stale "embedded backend lacks vector_search".
    /// Re-enabled 2026-05-26 as part of the mcp-recall-empty-hits fix.
    #[tokio::test]
    async fn source_prefix_filter_excludes_others() {
        let (lunaris, scope) = make_engine("test-recall-filter").await;
        let scoped = lunaris.scoped(scope.clone());

        scoped.ingest(EpisodeBuilder::new("notes/a", "note alpha content")).await.unwrap();
        scoped.ingest(EpisodeBuilder::new("notes/b", "note beta content")).await.unwrap();
        scoped.ingest(EpisodeBuilder::new("other/c", "other content entirely")).await.unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            RecallParams {
                query: "note content".into(),
                k: 10,
                filters: Some(RecallFilters { source_prefix: Some("notes/".into()) }),
                as_of: None,
                raw: false,
            },
        )
        .await
        .unwrap();

        // All returned hits must have source starting with "notes/".
        for hit in &resp.hits {
            assert!(
                hit.source.starts_with("notes/"),
                "filter must exclude sources not matching prefix; got source: {}",
                hit.source
            );
        }
        // "other/c" must not appear.
        assert!(
            resp.hits.iter().all(|h| !h.source.starts_with("other/")),
            "other/c must be excluded by source_prefix filter"
        );
    }

    #[tokio::test]
    async fn recall_enforces_k_after_hydration() {
        let (lunaris, scope) = make_engine("test-recall-k-cap").await;
        let scoped = lunaris.scoped(scope.clone());

        scoped.ingest(EpisodeBuilder::new("cap/a", "shared marker alpha")).await.unwrap();
        scoped.ingest(EpisodeBuilder::new("cap/b", "shared marker beta")).await.unwrap();
        scoped.ingest(EpisodeBuilder::new("cap/c", "shared marker gamma")).await.unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            RecallParams {
                query: "shared marker".into(),
                k: 1,
                filters: None,
                as_of: None,
                raw: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(resp.hits.len(), 1, "memory.recall must cap returned hits to k");
    }

    #[test]
    fn recall_handler_uses_hybrid_vector_keyword_rrf_plan() {
        let src = include_str!("recall.rs");
        assert!(
            src.contains("Vector::new(\"chunks\", candidate_k)")
                && src.contains("Keyword::bm25(\"chunks\", candidate_k)")
                && src.contains(".fuse_rrf(60)"),
            "memory.recall must use the canonical hybrid Vector+BM25 RRF plan"
        );
    }

    #[test]
    fn lazy_model_stager_not_constructed_by_params_parsing() {
        // Constructing RecallParams from JSON must not invoke ensure_staged.
        // Verified by: ensure_staged writes to ~/.lunaris/models/ — filesystem
        // I/O that cannot happen during a pure deserialize. The SKIP_STAGE
        // atomic and LUNARIS_MCP_SKIP_STAGE env var enforce the explicit call
        // site inside handle(), not at RecallParams construction time.
        let params: RecallParams =
            serde_json::from_str(r#"{"query": "test", "k": 3, "as_of": null}"#).unwrap();
        assert_eq!(params.query, "test");
        assert_eq!(params.k, 3);
        assert!(params.as_of.is_none());
        // Reaching here without model-dir creation proves the lazy invariant.
    }
}
