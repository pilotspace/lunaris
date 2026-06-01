//! RRF routing — Moon-native vs client-side fusion dispatch.
//!
//! Phase 1.5 STORE-09 added `RrfFusion::Moon { k, weights }` +
//! `StorageCapabilities::native_rrf` so a Moon backend can fuse vector + BM25
//! in **one** server round trip via `client.text().hybrid_search()`.
//! Plan 02-02's `fuse_rrf` consumes that capability:
//!
//! 1. [`inspect_branches`] walks the upstream operator tree to detect when
//!    both branches are exactly `Vector` + `Keyword(BM25)` on the SAME index.
//! 2. At call time, `fuse_rrf` checks `storage.capabilities().native_rrf` —
//!    when both conditions hold, dispatch to [`fuse_via_moon_native`] which
//!    calls into `MoonStorage::client().typed().text().hybrid_search()`.
//! 3. Otherwise the operator falls back to the client-side RRF in `fuse.rs`.
//!
//! ## Tree introspection mechanism
//!
//! The `Retriever` trait carries an `as_any() -> &dyn Any` method that every
//! concrete operator overrides with `fn as_any(&self) -> &dyn Any { self }`.
//! `inspect_branches` uses standard `Any::downcast_ref` to walk the tree —
//! no unsafe casts, no transmutes. Operators that don't override the
//! default impl downcast to `None`, which transparently routes the
//! dispatcher to the client-side fallback.
//!
//! ## Storage backend introspection
//!
//! Same pattern: an `AsStorageAny`-style blanket-impl trait lets us
//! recover `&dyn Any` from `&dyn StoragePort` without polluting the locked
//! Phase 1 trait. `fuse_via_moon_native` uses it to confirm the backend is
//! `MoonStorage` before invoking the Moon-native call.

use std::collections::HashMap;

use lunaris_core::LunarisError;
use lunaris_core::storage::keyword::min_max_normalize;
use lunaris_core::storage::types::Filter;

use crate::operators::QueryContext;
use crate::operators::Retriever;
use crate::operators::combinators::AndRetriever;
use crate::operators::graph::Graph;
use crate::operators::keyword::Keyword;
use crate::operators::vector::Vector;
use crate::types::{RawHit, SourceOp};

/// Tag describing what shape the fused branches take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FusedKind {
    /// Both branches are `Vector(index)` + `Keyword(index)` with the same index.
    VectorKeywordSameIndex,
    /// Anything else; will fall back to client-side fusion regardless of
    /// `native_rrf`.
    Other,
}

/// Hint extracted at `fuse_rrf` build time. Carries the index name so the
/// Moon-native dispatch knows which Moon FT index to query.
#[derive(Clone, Debug)]
pub struct FusedBranchHint {
    pub kind: FusedKind,
    pub index: String,
    pub k: usize,
}

/// Inspect the upstream operator tree. Returns `Some(hint)` when the tree is
/// the canonical "Vector + Keyword(BM25) on same index" shape that the
/// Moon-native path supports; `Some(Other)` when the tree IS an `And` but
/// the branches don't match the Moon-native pattern (still composable);
/// `None` when the inner is not an `And` at all (e.g., `fuse_rrf(Vector)` —
/// degenerate but still allowed for the client-side fold).
///
/// ## Plan 03-02: Graph operator forces client-side fusion
///
/// When EITHER branch is a [`Graph`] operator, returns `FusedKind::Other`
/// (not the Vector+Keyword native shape). The Moon-native
/// `text().hybrid_search()` path only fuses Vector + BM25; a Graph branch
/// must go through the client-side RRF fold so the Graph hits compose with
/// vector hits via the per-`SourceOp` group-and-rank flow. This also means
/// `Vector + Graph` (the canonical compose example from blueprint §8) NEVER
/// hits the Moon-native short-circuit even when both branches are wired to a
/// Moon backend with `native_rrf=true`.
pub fn inspect_branches(inner: &dyn Retriever) -> Option<FusedBranchHint> {
    let and = inner.as_any().downcast_ref::<AndRetriever>()?;

    // Plan 03-02 (D-15): Graph operator forces the client-side RRF path
    // because Moon-native hybrid_search only supports Vector + BM25 fusion.
    // Detect Graph in EITHER branch and short-circuit to FusedKind::Other.
    let left_is_graph = and.left.as_any().downcast_ref::<Graph>().is_some();
    let right_is_graph = and.right.as_any().downcast_ref::<Graph>().is_some();
    if left_is_graph || right_is_graph {
        return Some(FusedBranchHint { kind: FusedKind::Other, index: String::new(), k: 0 });
    }

    let left_v = and.left.as_any().downcast_ref::<Vector>();
    let right_k = and.right.as_any().downcast_ref::<Keyword>();
    match (left_v, right_k) {
        (Some(v), Some(k)) if v.index == k.index => Some(FusedBranchHint {
            kind: FusedKind::VectorKeywordSameIndex,
            index: v.index.clone(),
            k: v.k.max(k.k),
        }),
        (Some(v), Some(k)) => Some(FusedBranchHint {
            kind: FusedKind::Other,
            index: v.index.clone(),
            k: v.k.max(k.k),
        }),
        _ => Some(FusedBranchHint { kind: FusedKind::Other, index: String::new(), k: 0 }),
    }
}

/// Run the Moon-native hybrid search path.
///
/// Calls `MoonStorage::client().typed().text().hybrid_search(index, q_text,
/// q_vec, "vec", None, k, [bm25_w, vec_w, sparse_w])` — one round trip. Returns hits tagged
/// `SourceOp::Fused` with min-max normalized scores.
///
/// Reads the concrete `Arc<MoonStorage>` from `ctx.moon_storage`. The
/// caller (`FuseRrfRetriever::retrieve`) gates this with both
/// `capabilities().native_rrf == true` AND `ctx.moon_storage.is_some()`;
/// when either is false, dispatch falls back to client-side RRF.
pub async fn fuse_via_moon_native(
    ctx: &QueryContext,
    hint: &FusedBranchHint,
    k: usize,
    branch_weights: &HashMap<SourceOp, f32>,
) -> Result<Vec<RawHit>, LunarisError> {
    let moon = ctx.moon_storage.as_ref().ok_or_else(|| {
        LunarisError::Storage(lunaris_core::StorageError::Backend(
            "fuse_via_moon_native called without ctx.moon_storage; check builder wiring"
                .to_string(),
        ))
    })?;

    // Embed the query (cached in the OnceCell — same mechanism as Vector).
    let q_emb = ctx.embed_once().await?;

    let typed = moon.client().typed();
    let mut text = typed.text();
    // Resolve per-branch weights for Moon's RRF API (BM25 + dense, optional sparse).
    // The current SDK signature is `hybrid_search(..., sparse_field, k,
    // [w_bm25, w_dense, w_sparse])`, so we map:
    //   SourceOp::Keyword -> w_bm25
    //   SourceOp::Vector  -> w_dense
    // When the caller passes an empty weights map (default
    // `FuseRrfRetriever` with no `.with_weights(...)` call), preserve the
    // historical `[0.5, 0.5]` balanced default so existing call sites
    // observe NO behaviour change. Any explicit weight present in the map
    // overrides the 0.5 default; missing branches stay at 0.5.
    //
    // We pass `sparse_field: None` because Lunaris' default Moon indices expose
    // TEXT `content` + VECTOR `vec`; Moon performs the two-way fallback and
    // ignores `weights[2]`.
    let weights: [f64; 3] = if branch_weights.is_empty() {
        [0.5_f64, 0.5_f64, 0.0_f64]
    } else {
        let w_bm25 = branch_weights.get(&SourceOp::Keyword).copied().unwrap_or(0.5_f32) as f64;
        let w_dense = branch_weights.get(&SourceOp::Vector).copied().unwrap_or(0.5_f32) as f64;
        [w_bm25, w_dense, 0.0_f64]
    };
    let composite_query = compose_query_with_filter(&ctx.query.text, &ctx.query.filter);
    // RFC 0001 Wave 1C parity fix: the write path routes to the per-scope FT
    // index (`lunaris_{scope}_{kind}_idx`) but this hybrid-fusion read path
    // was still passing the bare `hint.index` ("chunks") to Moon — so it
    // queried an empty index and silently returned zero hits. Build the
    // per-scope name locally; `lunaris-storage-moon::keyspace::ft_index_name`
    // is the canonical helper but lives in a downstream crate we can't
    // depend on. The format is duplicated alongside `decode_moon_vector_key`
    // for the same reason. Live-measurement gap, 2026-05-12.
    let per_scope_index = format!("lunaris_{}_{}_idx", ctx.scope.as_str(), hint.index);
    let hits: Vec<moon::TextSearchHit> = text
        .hybrid_search(&per_scope_index, &composite_query, &q_emb, "vec", None, k, weights)
        .await
        .map_err(|e| {
            LunarisError::Storage(lunaris_core::StorageError::Backend(format!(
                "moon hybrid_search: {e}"
            )))
        })?;

    // Moon's HYBRID reply carries the fused score under `__rrf_score` in the
    // fields map — NOT in the SDK's typed `score` field. The Moon SDK parser
    // (`moon/sdk/rust/src/util.rs::parse_search_results`) only recognises
    // `__vec_score` / `vec_score`, so `h.score` is always 0.0 on the hybrid
    // path. Without this fallback, `min_max_normalize([0, 0, 0])` collapses
    // to `vec![1.0; n]` and every hit reports score=1.0 (live-measurement
    // gap, 2026-04-23). Prefer `__rrf_score` when present, fall back to the
    // typed `score` for non-hybrid code paths.
    let raw_scores: Vec<f32> = hits
        .iter()
        .map(|h| {
            h.fields
                .get("__rrf_score")
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(h.score as f32)
        })
        .collect();
    let normalized = min_max_normalize(&raw_scores);

    // Moon returns `<index>:<hex32>` keys per atomic.rs::VectorUpsert. Strip
    // the prefix and hex-decode to 16-byte ULID so `hydrate::chunk_lookup_key`
    // can recover the canonical `lunaris:chunk:<ulid>` lookup key. Same fix
    // as the vector/keyword paths in `lunaris-storage-moon` — hybrid was the
    // third silent-dropper (live-measurement gap, 2026-04-23).
    Ok(hits
        .into_iter()
        .zip(normalized)
        .filter_map(|(h, score)| {
            let raw_key = h.key.into_bytes();
            let id = decode_moon_vector_key(&raw_key, &per_scope_index)?;
            let metadata = h
                .fields
                .get("__metadata")
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .unwrap_or(serde_json::Value::Null);
            Some(RawHit {
                id,
                score,
                rerank_applied: false,
                degraded: false,
                metadata,
                source_op: SourceOp::Fused,
            })
        })
        .collect())
}

/// Strip `<index>:` prefix and hex-decode the rest to 16-byte ULID. Mirrors
/// `lunaris_storage_moon::vector::decode_key` — duplicated here because that
/// symbol is `pub(crate)` and this crate can't depend on lunaris-storage-moon
/// (would invert the dependency graph).
fn decode_moon_vector_key(key: &[u8], index: &str) -> Option<Vec<u8>> {
    let prefix_len = index.len() + 1;
    if key.len() < prefix_len || !key.starts_with(index.as_bytes()) || key[index.len()] != b':' {
        return None;
    }
    hex::decode(&key[prefix_len..]).ok().filter(|b| b.len() == 16)
}

/// Compose a text query with an optional filter for the Moon FT.SEARCH
/// hybrid_search path. When a filter is present the output is
/// `(<filter_expr>) <text>` so Moon applies the filter server-side before
/// RRF scoring. When no filter is set, returns the text unchanged.
fn compose_query_with_filter(text: &str, filter: &Option<Filter>) -> String {
    match filter {
        Some(f) => format!("({}) {}", filter_to_moon_tag(f), text),
        None => text.to_string(),
    }
}

/// Render a [`Filter`] tree as a Moon FT query expression for the hybrid
/// search path. TAG-aware: `source` field renders as `@source:{value}`
/// (TAG syntax per PERF-MOON-01), all other fields use TEXT syntax
/// `@field:"value"`.
///
/// Mirrors `keyword::filter_to_moon` — duplicated locally per the
/// codebase's established "extract when a third caller asks" convention
/// (keyword.rs and vector.rs each have their own copy already).
fn filter_to_moon_tag(f: &Filter) -> String {
    match f {
        Filter::Eq { field, value } => {
            if field == "source" {
                format!("@{field}:{{{}}}", ft_tag_escape(&json_bare(value)))
            } else {
                format!("@{field}:{}", json_quoted(value))
            }
        }
        Filter::StartsWith { field, prefix } => format!("@{field}:{prefix}*"),
        Filter::And(xs) => {
            format!("({})", xs.iter().map(filter_to_moon_tag).collect::<Vec<_>>().join(" "))
        }
        Filter::Or(xs) => {
            format!("({})", xs.iter().map(filter_to_moon_tag).collect::<Vec<_>>().join(" | "))
        }
        Filter::ValidTimeRange { after, before } => {
            let lo = after.map_or("-inf".to_string(), |h| h.wall_ms.to_string());
            let hi = before.map_or("+inf".to_string(), |h| h.wall_ms.to_string());
            format!("@valid_time:[{lo} {hi}]")
        }
        // `#[non_exhaustive]` on `Filter` requires a wildcard arm. New
        // variants need explicit fusion-side rendering; until that
        // lands, emit an unconstrained predicate and warn.
        _ => {
            tracing::warn!(
                "unknown Filter variant in retrieve fusion path — emitting unconstrained predicate"
            );
            "*".to_string()
        }
    }
}

/// Return the bare string value WITHOUT quotes — TAG values must not be quoted.
fn json_bare(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => format!("{v}"),
    }
}

/// Return the value with quotes for TEXT field syntax.
fn json_quoted(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => format!("\"{v}\""),
    }
}

/// Escape RediSearch TAG special characters with backslash.
/// Per RediSearch TAG rules: `,`, `.`, `{`, `}`, `\`, `:` must be escaped.
fn ft_tag_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        if matches!(ch, ',' | '.' | '{' | '}' | '\\' | ':') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operators::combinators::AndRetriever;
    use lunaris_core::storage::types::Filter;

    // ── filter_to_moon_tag tests (Plan 15-02) ──

    #[test]
    fn filter_to_moon_tag_source_renders_tag_syntax() {
        let f = Filter::Eq { field: "source".into(), value: serde_json::json!("notes.md") };
        assert_eq!(filter_to_moon_tag(&f), "@source:{notes\\.md}");
    }

    #[test]
    fn filter_to_moon_tag_source_escapes_colon() {
        let f = Filter::Eq { field: "source".into(), value: serde_json::json!("helios:fs/test") };
        assert_eq!(filter_to_moon_tag(&f), "@source:{helios\\:fs/test}");
    }

    #[test]
    fn filter_to_moon_tag_non_source_renders_text_syntax() {
        let f = Filter::Eq { field: "kind".into(), value: serde_json::json!("episode") };
        assert_eq!(filter_to_moon_tag(&f), "@kind:\"episode\"");
    }

    #[test]
    fn filter_to_moon_tag_and_combo() {
        let f = Filter::And(vec![
            Filter::Eq { field: "source".into(), value: serde_json::json!("x") },
            Filter::Eq { field: "kind".into(), value: serde_json::json!("y") },
        ]);
        assert_eq!(filter_to_moon_tag(&f), "(@source:{x} @kind:\"y\")");
    }

    #[test]
    fn compose_query_with_filter_prepends_filter() {
        let result = compose_query_with_filter(
            "hello",
            &Some(Filter::Eq { field: "source".into(), value: serde_json::json!("x") }),
        );
        assert_eq!(result, "(@source:{x}) hello");
    }

    #[test]
    fn compose_query_with_filter_none_returns_text() {
        let result = compose_query_with_filter("hello", &None);
        assert_eq!(result, "hello");
    }

    // ── inspect_branches tests ──

    #[test]
    fn inspect_recognizes_vector_plus_keyword_same_index() {
        let v = Vector::new("chunks", 30);
        let k = Keyword::bm25("chunks", 30);
        let and = AndRetriever::new(Box::new(v), Box::new(k));
        let hint = inspect_branches(&and).expect("must detect");
        assert_eq!(hint.kind, FusedKind::VectorKeywordSameIndex);
        assert_eq!(hint.index, "chunks");
    }

    #[test]
    fn inspect_recognizes_different_index_as_other() {
        let v = Vector::new("chunks", 30);
        let k = Keyword::bm25("entities", 30);
        let and = AndRetriever::new(Box::new(v), Box::new(k));
        let hint = inspect_branches(&and).expect("AND visible");
        assert_eq!(hint.kind, FusedKind::Other);
    }

    #[test]
    fn inspect_returns_none_for_non_and() {
        let v = Vector::new("chunks", 30);
        // Not wrapped in AND — just a bare vector. inspect_branches must return None.
        let hint = inspect_branches(&v);
        assert!(hint.is_none());
    }

    #[test]
    fn inspect_forces_other_when_left_branch_is_graph() {
        // Plan 03-02 (D-15): Graph operator must force the client-side RRF
        // path because Moon-native hybrid_search only supports Vector + BM25
        // fusion. Vector + Graph MUST NOT be detected as
        // VectorKeywordSameIndex (which would silently route to Moon-native).
        let g = Graph::anchored(Vec::<(lunaris_extract::EntityId, f32)>::new(), 2);
        let v = Vector::new("chunks", 30);
        let and = AndRetriever::new(Box::new(g), Box::new(v));
        let hint = inspect_branches(&and).expect("AND visible");
        assert_eq!(hint.kind, FusedKind::Other, "Graph branch must force client-side fusion path",);
    }

    #[test]
    fn inspect_forces_other_when_right_branch_is_graph() {
        // Symmetric to the left-branch case — caller order MUST NOT matter.
        // The canonical compose example from blueprint §8 puts Vector first
        // and Graph second: Vector::new(...).and(Graph::anchored(...)).
        let v = Vector::new("chunks", 30);
        let g = Graph::anchored(Vec::<(lunaris_extract::EntityId, f32)>::new(), 2);
        let and = AndRetriever::new(Box::new(v), Box::new(g));
        let hint = inspect_branches(&and).expect("AND visible");
        assert_eq!(
            hint.kind,
            FusedKind::Other,
            "Graph branch must force client-side fusion path regardless of position",
        );
    }
}
