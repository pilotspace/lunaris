//! `memory.recall` — semantic retrieval over agent memory.
//!
//! Wave 2.B: full implementation.
//! - Lazy model stager (`ensure_staged`) on first call, not at server start.
//! - `Query::text(query).k` + optional `Filter::StartsWith` + optional `as_of`.
//! - Maps `Vec<Hit>` → `Vec<RecallHit>` with ≤ 200-char content snippet.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::DateTime;
use lunaris::Query;
use lunaris_core::{Hlc, storage::types::Filter};
use serde::{Deserialize, Serialize};

use crate::model_stager::{ModelKind, StageError, ensure_staged};
use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Optional filters for `memory.recall`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecallFilters {
    /// Only return memories whose source starts with this prefix.
    #[serde(default)]
    pub source_prefix: Option<String>,
}

/// Input parameters for `memory.recall`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecallParams {
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
}

fn default_k() -> usize {
    5
}

/// A single recalled memory hit.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct RecallHit {
    /// Episode ID (ULID string).
    pub episode_id: String,
    /// Logical source of the episode.
    pub source: String,
    /// Text content of the recalled episode (≤ 200 chars).
    pub content: String,
    /// Combined retrieval score ∈ [0, 1].
    pub score: f32,
    /// Ingest wall-clock timestamp in RFC-3339.
    pub ingested_at: String,
}

/// Output of a successful `memory.recall` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct RecallResponse {
    /// Ordered list of recalled memories (highest score first).
    pub hits: Vec<RecallHit>,
}

// ── Lazy stager ───────────────────────────────────────────────────────────────

/// Printed at most once per process on the first model download.
static STAGE_LOG_ONCE: OnceLock<()> = OnceLock::new();

/// Test seam: when `true`, `maybe_ensure_staged` skips the real GGUF download.
///
/// Set by `#[cfg(test)]` helpers via `skip_stage_for_tests()`. Production code
/// never sets this — the value starts `false` and only flips inside the test
/// binary. This avoids `unsafe { std::env::set_var }` under `forbid(unsafe_code)`.
static SKIP_STAGE: AtomicBool = AtomicBool::new(false);

/// Enable the staging bypass in the current process (test-only call site).
#[cfg(test)]
fn skip_stage_for_tests() {
    SKIP_STAGE.store(true, Ordering::Relaxed);
}

/// Ensure the embedder GGUF is staged — called lazily from the first recall.
///
/// Bypassed when:
/// - `LUNARIS_MCP_SKIP_STAGE` env var is set (CI / operator override), OR
/// - `SKIP_STAGE` atomic is `true` (test seam — no `unsafe` needed).
async fn maybe_ensure_staged() -> Result<(), ToolError> {
    if SKIP_STAGE.load(Ordering::Relaxed)
        || std::env::var_os("LUNARIS_MCP_SKIP_STAGE").is_some()
    {
        return Ok(());
    }
    STAGE_LOG_ONCE.get_or_init(|| {
        eprintln!("lunaris-mcp: staging models — first run only");
    });
    ensure_staged(ModelKind::EmbedderGraniteQ4KM).await.map(|_| ()).map_err(|e: StageError| {
        ToolError::InvalidInput(format!("model staging failed: {e}"))
    })
}

// ── Hlc helpers ──────────────────────────────────────────────────────────────

fn parse_rfc3339_to_hlc(s: &str) -> Result<Hlc, ToolError> {
    let dt = DateTime::parse_from_rfc3339(s)
        .map_err(|e| ToolError::InvalidInput(format!("as_of is not valid RFC-3339: {e}")))?;
    let wall_ms = u64::try_from(dt.timestamp_millis())
        .map_err(|_| ToolError::InvalidInput("as_of timestamp predates Unix epoch".into()))?;
    Ok(Hlc { wall_ms, counter: 0, node_id: 0 })
}

fn hlc_to_rfc3339(hlc: &Hlc) -> String {
    DateTime::from_timestamp_millis(hlc.wall_ms as i64)
        .unwrap_or_default()
        .to_rfc3339()
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.recall`.
///
/// 1. Stage the embedder GGUF via `model_stager::ensure_staged` (lazy — once
///    per process; stderr-only progress bar; bypassed in tests).
/// 2. Build `Query::text(query)` with optional `k`, `Filter::StartsWith`, and
///    `as_of` bi-temporal snapshot.
/// 3. Delegate to `ScopedLunaris::recall(query)` — scope-partitioned.
/// 4. Map each `Hit` to `RecallHit` (content truncated at 200 chars).
pub(crate) async fn handle(
    state: &AppState,
    params: RecallParams,
) -> Result<RecallResponse, ToolError> {
    // Lazy model stager — first recall pays the download cost. Subsequent
    // calls hit the sha-verified fast path in ensure_staged (<1 ms).
    maybe_ensure_staged().await?;

    // Empty query → empty hits (not an error).
    if params.query.is_empty() {
        return Ok(RecallResponse { hits: vec![] });
    }

    // Treat k=0 as default_k so callers can omit the field via `k: 0`.
    let k = if params.k == 0 { default_k() } else { params.k };

    // Build the query.
    let mut q = Query::text(&params.query);
    q.k = k;

    // Optional source-prefix filter.
    if let Some(filters) = &params.filters {
        if let Some(prefix) = &filters.source_prefix {
            if !prefix.is_empty() {
                q.filter =
                    Some(Filter::StartsWith { field: "source".into(), prefix: prefix.clone() });
            }
        }
    }

    // Optional bi-temporal as_of.
    if let Some(ts) = &params.as_of {
        q.as_of = Some(parse_rfc3339_to_hlc(ts)?);
    }

    // Execute — ScopedLunaris::recall threads the bound scope through.
    let scoped = state.lunaris.scoped(state.scope.clone());
    let hits = scoped.recall(q).await.map_err(ToolError::LunarisEngine)?;

    // Map hits to wire DTO with explicit type so the collector infers correctly.
    let recall_hits: Vec<RecallHit> = hits
        .into_iter()
        .map(|h| {
            let episode_id = ulid_bytes_to_string(&h.id);
            let content: String = h.text.chars().take(200).collect();
            let ingested_at = hlc_to_rfc3339(&h.valid_from);
            RecallHit { episode_id, source: h.source, content, score: h.score, ingested_at }
        })
        .collect();

    tracing::debug!(
        scope = state.scope.as_str(),
        k     = k,
        hits  = recall_hits.len(),
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris::{EpisodeBuilder, Lunaris};
    use lunaris_core::{Scope, StubEmbedder};

    use super::*;
    use crate::state::AppState;

    /// Construct an in-process Lunaris handle with `StubEmbedder` (deterministic
    /// 768-d vectors) backed by SQLite `memory://`. StubEmbedder produces
    /// deterministic non-zero vectors so vector recall returns real ranked hits
    /// rather than the zero-vector NoopEmbedder which returns nothing.
    async fn fresh_state(scope_name: &str) -> AppState {
        // Bypass the 253 MB HF download in tests — StubEmbedder provides recall.
        skip_stage_for_tests();
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder("memory://", embedder).await.unwrap();
        let scope = Scope::new(scope_name).unwrap();
        AppState { lunaris: Arc::new(lunaris), scope }
    }

    #[tokio::test]
    #[ignore = "embedded backend lacks vector_search; run with LUNARIS_STORAGE_URL=redis://localhost:6380 cargo test -p lunaris-mcp -- --ignored"]
    async fn ingest_then_recall_round_trip() {
        let state = fresh_state("test-recall-rt").await;
        let scoped = state.lunaris.scoped(state.scope.clone());

        scoped
            .ingest(EpisodeBuilder::new("test/src", "chocolate cake recipe with cocoa"))
            .await
            .unwrap();

        let resp = handle(
            &state,
            RecallParams { query: "chocolate".into(), k: 5, filters: None, as_of: None },
        )
        .await
        .unwrap();

        assert!(!resp.hits.is_empty(), "expected at least one hit");
        let top = &resp.hits[0];
        assert!(!top.episode_id.is_empty());
        assert!(!top.ingested_at.is_empty());
    }

    #[tokio::test]
    #[ignore = "embedded backend lacks vector_search; run with LUNARIS_STORAGE_URL=redis://localhost:6380 cargo test -p lunaris-mcp -- --ignored"]
    async fn as_of_time_travel_proves_bi_temporal() {
        let state = fresh_state("test-recall-bt").await;
        let scoped = state.lunaris.scoped(state.scope.clone());

        // Ingest fact A, capture wall time between the two ingests.
        scoped
            .ingest(EpisodeBuilder::new("bt/src", "the sky is blue"))
            .await
            .unwrap();

        // t1.5 is after A but before B — use current time as the as_of anchor.
        let t_between = chrono::Utc::now().to_rfc3339();
        // Small sleep so B's HLC wall_ms is strictly after t_between.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        scoped
            .ingest(EpisodeBuilder::new("bt/src", "the sky is green"))
            .await
            .unwrap();

        // Recall with as_of = t_between: B was not ingested yet at that point.
        let resp = handle(
            &state,
            RecallParams {
                query: "sky".into(),
                k: 5,
                filters: None,
                as_of: Some(t_between),
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

    #[tokio::test]
    #[ignore = "embedded backend lacks vector_search; run with LUNARIS_STORAGE_URL=redis://localhost:6380 cargo test -p lunaris-mcp -- --ignored"]
    async fn source_prefix_filter_excludes_others() {
        let state = fresh_state("test-recall-filter").await;
        let scoped = state.lunaris.scoped(state.scope.clone());

        scoped
            .ingest(EpisodeBuilder::new("notes/a", "note alpha content"))
            .await
            .unwrap();
        scoped
            .ingest(EpisodeBuilder::new("notes/b", "note beta content"))
            .await
            .unwrap();
        scoped
            .ingest(EpisodeBuilder::new("other/c", "other content entirely"))
            .await
            .unwrap();

        let resp = handle(
            &state,
            RecallParams {
                query: "note content".into(),
                k: 10,
                filters: Some(RecallFilters { source_prefix: Some("notes/".into()) }),
                as_of: None,
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
