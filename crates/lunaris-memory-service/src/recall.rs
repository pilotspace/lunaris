//! `memory.recall` — semantic retrieval over agent memory.
//!
//! Wave 2.B: full implementation.
//! - Lazy model stager (`ensure_staged`) on first call, not at server start.
//! - `Query::text(query).k` + optional `as_of`; `source_prefix` is enforced
//!   AFTER recall on the hydrated Episode source (never pushed into storage).
//! - Maps `Vec<Hit>` → `Vec<RecallHit>` with a curated LLM-ready snippet
//!   (`lunaris_core::snippet`, ≤ 260 chars) by default, or the raw stored
//!   bytes (≤ 200 chars) when `raw: true`.

use std::sync::Arc;

use chrono::DateTime;
use lunaris::Query;
use lunaris_core::{Hlc, snippet};
use lunaris_retrieve::{
    LedgerBoostProvider, Reranker, RetrievalBuilder, Retriever, Vector, production_root,
    production_root_reranked,
};
use serde::{Deserialize, Serialize};

use crate::ServiceError;
use crate::is_keyword_not_supported;
use lunaris::Lunaris;
use lunaris_core::Scope;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Optional filters for `memory.recall`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallFilters {
    /// Only return memories whose source starts with this prefix.
    #[serde(default)]
    pub source_prefix: Option<String>,
}

/// Input parameters for `memory.recall`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
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
/// 2. Build `Query::text(query)` with optional `k` and `as_of` bi-temporal
///    snapshot. `source_prefix` is NOT pushed into storage — it is enforced
///    after recall on the hydrated source (see step 4), backed by an 8×-widened
///    candidate over-fetch, because backend push-down filtering is lossy on
///    Moon.
/// 3. Execute the GA-1 unified production root ([`recall_root`] →
///    `lunaris_retrieve::production_root`): `Vector ∧ BM25("chunks") →
///    fuse_rrf(60) → top(candidate_k)`, plus the fact legs
///    (`Navigate("entities", fallback "facts") ∧ BM25("facts")`) when the
///    graph pipeline is enabled, plus the opt-in `LUNARIS_RECALL_RERANK`
///    cross-encoder stage between fusion and the final top-k. Moon handles
///    the chunks fusion as native HYBRID search when the backend supports
///    it; other keyword-capable backends use client-side RRF. Backends
///    without a keyword surface fall back narrowly to vector-only recall.
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

    // Honesty gate (unified-inference, 2026-07-19): recall needs a real
    // embedder — a NoopEmbedder produces a zero query vector, `vector_search`
    // short-circuits on it, and the caller gets a silent empty hit list (the
    // `mcp-recall-empty-hits` bug). Since the lazy embedder no longer
    // hard-errors (so ingest can still store KV + BM25 without weights), the
    // loud "no embedder" error lives HERE, on the one path where a zero vector
    // is a lie. One probe per process; latches ready on the first real vector.
    ensure_embedder_ready(lunaris).await?;

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

    // Do NOT push source_prefix down into the storage branches. On Moon that
    // push-down is silently lossy — the keyword branch renders `@source:pre*`
    // (invalid TAG syntax; `source` is a TAG field, and pre-PERF-MOON-01 indexes
    // lack the field entirely) and the vector branch's over-fetch post-filter
    // returns empty on large/quantized indexes. A backend that HONORS a lossy
    // push-down returns ZERO candidates, so the authoritative post-filter below
    // never sees the match — the live git_487b86f2 `memory.recall` bug
    // (2026-07-20): same candidate set returned 0 hits pushed-down vs 22 when
    // post-filtered on the hydrated source. The prefix is enforced ONLY after
    // recall, on the hydrated Episode `source` (see the filter at the map step),
    // backed by the 8×-widened `candidate_k` over-fetch above. Regression:
    // tests/recall_source_prefix_moonlike.rs.

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
    // GA-1: the root is built THROUGH the `recall_root` seam (→
    // `lunaris_retrieve::production_root`), so the graph-ON fact legs are no
    // longer silently discarded by this surface's `with_root` (the pre-GA-1
    // bug) and the opt-in rerank stage composes identically to HTTP/SDK.
    let scoped = lunaris.scoped(scope.clone());
    let graph_enabled = lunaris.graph_pipeline().is_enabled();
    let rerank_cfg = lunaris.recall_rerank();
    let rerank_arg = rerank_cfg.enabled.then(|| (lunaris.reranker(), rerank_cfg.top_in));
    let root = recall_root(candidate_k, graph_enabled, rerank_arg);
    let hits = match with_activation_boost(scoped.dsl().with_root_boxed(root), lunaris)
        .execute(q.clone())
        .await
    {
        Ok(hits) => hits,
        Err(err) if is_keyword_not_supported(&err) => {
            tracing::debug!(
                scope = scope.as_str(),
                "memory.recall keyword branch unsupported; falling back to vector-only"
            );
            with_activation_boost(
                scoped.dsl().with_root(Vector::new("chunks", candidate_k)),
                lunaris,
            )
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
            // Episode-grain id contract (2026-07-19 fix): the wire field is
            // the PARENT EPISODE's ULID — the id memory.feedback /
            // memory.forget / dream-agenda hydration all operate on. Fact
            // hits (and any non-hydrated path) have an empty provenance
            // channel; fall back to the raw hit id for those.
            let episode_id = if h.episode_id.is_empty() {
                ulid_bytes_to_string(&h.id)
            } else {
                ulid_bytes_to_string(&h.episode_id)
            };
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

/// Process-wide "the embedder can produce real vectors" latch.
///
/// The lazy default embedder (`lunaris::lazy_default_embedder`) tolerates a
/// `NoopEmbedder` fallback so ingest still writes KV + BM25 without weights.
/// That tolerance would let recall return a silent empty hit list from a zero
/// query vector — the `mcp-recall-empty-hits` bug the old boot probe guarded.
/// This restores that guard, lazily on the recall path: probe the embedder
/// with a tiny input; a zero vector → an actionable error. The latch flips
/// `true` on the first real (non-zero) vector so steady-state recall pays
/// nothing; it never latches on failure, so a caller that stages weights
/// between calls (the mcp model stager) recovers on the next recall.
async fn ensure_embedder_ready(lunaris: &Lunaris) -> Result<(), ServiceError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static READY: AtomicBool = AtomicBool::new(false);
    if READY.load(Ordering::Relaxed) {
        return Ok(());
    }
    let embedder = lunaris.embedder();
    let probe = embedder
        .embed_batch(&["lunaris recall embedder health probe"])
        .await
        .map_err(ServiceError::LunarisEngine)?;
    let v = probe.into_iter().next().unwrap_or_default();
    if v.is_empty() || v.iter().all(|&x| x == 0.0_f32) {
        return Err(ServiceError::InvalidInput(
            "no embedding backend available: the embedder resolved to NoopEmbedder \
             (zero vectors), so semantic recall cannot run. Set LUNARIS_EMBEDDER_GGUF=\
             <path to a granite-embedding-311m-multilingual-r2 GGUF> or stage weights \
             under ~/.lunaris/models/. (Ingest still works without an embedder — only \
             vector recall requires one.)"
                .to_owned(),
        ));
    }
    READY.store(true, Ordering::Relaxed);
    Ok(())
}

/// GA-1 — the `memory.recall` root-construction seam.
///
/// The handler builds its root EXCLUSIVELY through this function, which
/// delegates to the canonical [`lunaris_retrieve::production_root`] /
/// [`lunaris_retrieve::production_root_reranked`] compositions. Pinned by
/// `tests/recall_root_conformance.rs` (plan equality against
/// `production_root` for every (graph, rerank) combination) so a future
/// inline `with_root` divergence on this surface fails a NAMED test — the
/// pre-GA-1 handler built a private chunks-only root that silently dropped
/// the graph-ON fact legs.
///
/// `rerank` is `Some((reranker, top_in_override))` ONLY when the handle's
/// `LUNARIS_RECALL_RERANK` toggle is on; passing `None` provably never
/// touches the reranker (the lazy GGUF stays unloaded).
pub fn recall_root(
    candidate_k: usize,
    graph_enabled: bool,
    rerank: Option<(Arc<dyn Reranker>, Option<usize>)>,
) -> Box<dyn Retriever> {
    match rerank {
        Some((reranker, top_in)) => {
            Box::new(production_root_reranked(candidate_k, graph_enabled, reranker, top_in))
        }
        None => Box::new(production_root(candidate_k, graph_enabled)),
    }
}

/// ADD task activation-ledger — wire the persistent activation-ledger prior
/// into the SERVICE recall path (the path the hook and MCP tools actually
/// execute; built≠wired). Opt-out via `LUNARIS_ACTIVATION_BOOST=0`; the
/// default SDK `Lunaris::recall()` / `.dsl()` surfaces stay unwired per the
/// frozen contract, so library consumers are untouched.
fn with_activation_boost(builder: RetrievalBuilder, lunaris: &Lunaris) -> RetrievalBuilder {
    let enabled = std::env::var("LUNARIS_ACTIVATION_BOOST").map(|v| v != "0").unwrap_or(true);
    if !enabled {
        return builder;
    }
    builder.with_boost_provider(Arc::new(LedgerBoostProvider::new(lunaris.storage())))
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
    use lunaris_core::{HlcClock, Scope, StubEmbedder};
    use lunaris_test_harness::doubles::{NoKeywordSearch, PortWithCaps};
    use lunaris_test_harness::{
        TestEngine, TestStorage, open_test_engine_with_embedder, open_test_storage,
    };

    /// Construct an in-process Lunaris handle with `StubEmbedder` (deterministic
    /// 768-d vectors) over a harness-issued ephemeral Moon. StubEmbedder
    /// produces deterministic non-zero vectors so vector recall returns real
    /// ranked hits rather than the zero-vector NoopEmbedder which returns
    /// nothing.
    ///
    /// `TestEngine` derefs to `Lunaris`, so call sites are unchanged — but the
    /// binding owns the Moon child and must outlive them.
    async fn make_engine(scope_name: &str) -> (TestEngine, Scope) {
        // No staging needed — the handler no longer stages (that is a CALLER
        // concern, lifted to the mcp/contextd boundary). StubEmbedder provides
        // deterministic recall without the 253 MB HF download.
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = open_test_engine_with_embedder(embedder).await;
        let scope = Scope::new(scope_name).unwrap();
        (lunaris, scope)
    }

    /// An engine on a real Moon that is forced down the CLIENT-SIDE fusion
    /// path: `with_parts_keyword` leaves `moon_storage = None` (so
    /// `fuse_rrf` cannot take Moon's one-round-trip `HYBRID`), and
    /// [`NoKeywordSearch`] removes the BM25 leg.
    ///
    /// That combination is what the activation-boost test needs and why it
    /// exists — see its doc comment.
    async fn make_client_side_fusion_engine(scope_name: &str) -> (Lunaris, Scope, TestStorage) {
        let storage = open_test_storage().await;
        let engine = Lunaris::with_parts_keyword(
            Arc::new(PortWithCaps::new(storage.port(), |_| {})),
            Arc::new(NoKeywordSearch),
            Arc::new(StubEmbedder::new(768)),
            HlcClock::new(0),
        );
        (engine, Scope::new(scope_name).unwrap(), storage)
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

    /// ADD task activation-ledger — built≠wired proof for the SERVICE recall
    /// path: a strong-reinforced memory must outrank its equal-similarity
    /// peer through `handle()` itself (the exact path the hook / MCP tools
    /// execute), not just through an explicitly-wired test builder.
    ///
    /// **Re-expressed in 0.7.0 against port doubles.** This used to open the
    /// embedded backend, and the comment claimed that backend gave genuine
    /// score ties. It did not: it gave CLIENT-SIDE RRF scores, `1/(60+1)` vs
    /// `1/(60+2)` — a 1.6% rank gap that the ledger prior clears every time.
    /// Moon's *native* one-round-trip fusion returns a ~10x gap for the same
    /// pair (1.0 vs 0.1027), which the prior clears only sometimes; left
    /// unpinned the test failed ~1 run in 5 (measured 1/5, then 3/8).
    ///
    /// So the honest subject is the client-side fusion path, and the fixture
    /// now says so out loud instead of naming a backend:
    /// [`make_client_side_fusion_engine`] runs a REAL Moon with
    /// `moon_storage = None` and a [`NoKeywordSearch`] double, which is
    /// exactly the arrangement the deleted backend produced.
    ///
    /// The Moon-native gap is NOT covered here and must not be read as
    /// covered. It is a production question — whether `LUNARIS_ACTIVATION_BOOST`
    /// (default-on) means anything against Moon's rank-based fusion — tracked
    /// as item 7 of docs/testing/memory-to-moon-port-plan.md §6, deliberately
    /// out of scope for the deletion.
    #[tokio::test]
    async fn activation_boost_reranks_client_side_fused_recall() {
        use lunaris_core::activation::{Grain, RefSignal, Strength};

        let (lunaris, scope, _storage) =
            make_client_side_fusion_engine("test-recall-actboost").await;
        let scoped = lunaris.scoped(scope.clone());

        // Two distinct episodes with IDENTICAL content → identical vectors
        // (StubEmbedder is deterministic per text) and identical BM25.
        for _ in 0..2 {
            scoped
                .ingest(EpisodeBuilder::new("test/src", "granite embedding activation corpus"))
                .await
                .unwrap();
        }

        let params = || RecallParams {
            query: "granite activation corpus".into(),
            k: 2,
            filters: None,
            as_of: None,
            raw: false,
        };
        let base = handle(&lunaris, &scope, params()).await.unwrap();
        assert_eq!(base.hits.len(), 2, "need both equal-similarity hits: {base:?}");
        let loser = base.hits[1].episode_id.clone();

        // Strong-reinforce the CURRENTLY-SECOND hit; only the ledger prior
        // can explain a flip.
        let id = ulid::Ulid::from_string(&loser).unwrap();
        scoped
            .record_activation_refs(&[
                RefSignal { id, grain: Grain::Turn, strength: Strength::Strong },
                RefSignal { id, grain: Grain::ToolCall, strength: Strength::Strong },
            ])
            .await
            .unwrap();

        let after = handle(&lunaris, &scope, params()).await.unwrap();
        assert_eq!(
            after.hits[0].episode_id, loser,
            "reinforced memory must outrank its equal-similarity peer through \
             the service recall path (LUNARIS_ACTIVATION_BOOST default-on): {after:?}"
        );
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

    /// **Re-expressed in 0.7.0 as the capability-honest arm.**
    ///
    /// The old test ingested A, captured a wall time, ingested B, and asserted
    /// a recall pinned to `t_between` did not surface B. That claim needed a
    /// bi-temporal backend, and the workspace no longer has one: the embedded
    /// SQLite and Postgres backends that could answer a historical read were
    /// both deleted, and Moon reads current state only (STORE-07).
    ///
    /// Faking it would be the worst outcome — a caller who believes they got a
    /// historical view and acts on current-state data. So what is pinned here
    /// is the REFUSAL, end to end through the same `handle` the MCP tools and
    /// the hook call: `as_of` on a non-bi-temporal backend must return
    /// `InvalidInput` naming STORE-07, never hits.
    ///
    /// This is strictly stronger than the pure-function test in
    /// `as_of_gate_tests`, which proves `ensure_as_of_supported` is correct but
    /// not that the service path calls it (built ≠ wired). The positive
    /// direction — a `bi_temporal_native = true` backend passes the gate —
    /// stays covered there, against synthetic capabilities, which is the only
    /// honest way to express it with no such backend in the tree.
    #[tokio::test]
    async fn as_of_on_a_non_bitemporal_backend_is_refused_not_faked() {
        let (lunaris, scope) = make_engine("test-recall-bt").await;
        assert!(
            !lunaris.storage().capabilities().bi_temporal_native,
            "precondition: the only backend 0.7.0 ships is not bi-temporal"
        );
        let scoped = lunaris.scoped(scope.clone());

        // Ingest fact A, capture wall time between the two ingests.
        scoped.ingest(EpisodeBuilder::new("bt/src", "the sky is blue")).await.unwrap();

        // t1.5 is after A but before B — use current time as the as_of anchor.
        let t_between = chrono::Utc::now().to_rfc3339();
        // Small sleep so B's HLC wall_ms is strictly after t_between.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        scoped.ingest(EpisodeBuilder::new("bt/src", "the sky is green")).await.unwrap();

        let err = handle(
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
        .expect_err("as_of must be refused, not silently answered with current state");

        let msg = err.to_string();
        assert!(
            msg.contains("bi-temporal") && msg.contains("STORE-07"),
            "the refusal must name the capability and the tracking id, got: {msg}"
        );
        assert!(!msg.contains("green"), "the refusal must not leak current-state content: {msg}");
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

    /// Collect the trailing `{ulid}` of every KV key under `prefix`.
    async fn scan_trailing_ulids(
        lunaris: &Lunaris,
        scope: &Scope,
        prefix: Vec<u8>,
    ) -> Vec<ulid::Ulid> {
        use futures::StreamExt;
        let mut out = Vec::new();
        let storage = lunaris.storage();
        let mut stream = storage.scan_range(scope, &prefix, None).await.unwrap();
        while let Some(item) = stream.next().await {
            let (key, _) = item.unwrap();
            let s = std::str::from_utf8(&key).unwrap();
            let tail = &s[s.rfind(':').unwrap() + 1..];
            if let Ok(id) = ulid::Ulid::from_string(tail) {
                out.push(id);
            }
        }
        out
    }

    /// engram id-space fix (2026-07-19 live finding) — the wire `episode_id`
    /// MUST carry the PARENT EPISODE's ULID (the id `memory.feedback`,
    /// `memory.forget`, and dream-agenda hydration all operate on), never the
    /// chunk's own id. Before the fix, recall returned chunk ULIDs labeled
    /// `episode_id`, so feedback votes landed on chunk-keyed ledger rows that
    /// `memory.dream_agenda` silently dropped.
    #[tokio::test]
    async fn recall_episode_id_is_parent_episode_not_chunk_id() {
        let (lunaris, scope) = make_engine("test-recall-episode-grain").await;
        let scoped = lunaris.scoped(scope.clone());
        scoped
            .ingest(EpisodeBuilder::new("test/src", "episode grain wire contract fixture"))
            .await
            .unwrap();

        let episode_ids =
            scan_trailing_ulids(&lunaris, &scope, lunaris_core::keyspace::episode_prefix(&scope))
                .await;
        let chunk_ids =
            scan_trailing_ulids(&lunaris, &scope, lunaris_core::keyspace::chunk_prefix(&scope))
                .await;
        assert_eq!(episode_ids.len(), 1, "exactly one episode ingested");
        assert!(!chunk_ids.is_empty(), "ingest must have produced chunks");
        assert!(
            !chunk_ids.contains(&episode_ids[0]),
            "episode and chunk ids must be distinct ULIDs for this test to discriminate"
        );

        let resp = handle(
            &lunaris,
            &scope,
            RecallParams {
                query: "episode grain wire contract".into(),
                k: 5,
                filters: None,
                as_of: None,
                raw: false,
            },
        )
        .await
        .unwrap();
        let hit = resp.hits.first().expect("recall must return the ingested episode");
        assert_eq!(
            hit.episode_id,
            episode_ids[0].to_string(),
            "wire episode_id must be the parent episode ULID, not the chunk id"
        );
    }
}
