//! `vector_search` — Moon `FT.SEARCH` KNN over per-scope vector indices.
//!
//! RFC 0001 Wave 1C: the FT index consulted is the per-scope index
//! `ft_index_name(scope, index)` (e.g. `lunaris_acme:agent-1_chunks_idx`).
//! `decode_key` strips the per-scope FT index name prefix (not the bare `index`
//! name) before hex-decoding the ULID — the 16-byte ULID contract is preserved.
//!
//! ## AS_OF semantics
//!
//! When `as_of = Some(t)`, Lunaris emits Moon's native
//! `FT.SEARCH ... AS_OF <t.wall_ms>` clause on the same search command. We do not
//! pre-pin the connection with `TEMPORAL.SNAPSHOT_AT`; Moon's FT parser owns the
//! temporal lookup for vector/keyword search.
//!
//! ## Filter algebra (ft-navigate-filter-gap contract v1.1, 2026-07-14)
//!
//! Moon's FT.SEARCH inline filter grammar (`parse_filter_string`,
//! vendor/moon ft_search/parse.rs) accepts ONLY space-joined
//! `@field:{tag}` / `@field:[min max]` units — no parens, no `|` OR, no
//! prefix-wildcard, no unescaping. The pre-v1.1 rendering
//! (`({filter})=>[KNN…]` + backslash TAG escaping) was SILENTLY DROPPED by
//! that parser (leading `(` aborts the parse — live-probed 2026-07-14).
//!
//! v1.1 semantics:
//! - server-renderable subset (`chunks` index only — the sole index with
//!   TAG/NUMERIC schema fields): And-composition of `Eq{source}` (raw,
//!   single-token value) and `ValidTimeRange` — rendered space-joined with
//!   NO parens and NO escaping (`render_knn_filter`).
//! - everything else: over-fetch the KNN unfiltered (`k*4` clamped to
//!   `[k, 1000]`) and post-filter client-side against `VectorHit.metadata`
//!   (`filter_matches`), truncating to `k`. Unknown `Filter` variants are a
//!   hard `StorageError::Backend` — never a silent drop.

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{Filter, VectorHit};

use crate::client::{MoonClient, moon_err, redis_err};
use crate::keyspace::ft_index_name;

/// The four Lunaris-owned per-scope vector index kinds. Mirrors the list in
/// `client.rs::ensure_indexes` / `lib.rs::create_scope_indexes` — kept as a
/// standalone const here (rather than importing theirs) because neither is
/// `pub` and this crate's file-ownership split (moon-v051-perf-exploit W1)
/// keeps this module independent of `lib.rs`.
pub(crate) const SCOPE_VECTOR_INDEX_KINDS: [&str; 4] =
    ["chunks", "entities", "facts", "communities"];

#[allow(clippy::too_many_arguments)]
pub(crate) async fn vector_search(
    c: &MoonClient,
    scope: &Scope,
    index: &str,
    query: &[f32],
    k: usize,
    filter: Option<&Filter>,
    as_of: Option<Hlc>,
    rerank: bool,
) -> Result<Vec<VectorHit>, StorageError> {
    // RFC 0001 Wave 1C: route to the per-scope FT index.
    let per_scope_index = ft_index_name(scope, index);

    // Encode query embedding as little-endian f32 bytes per Moon FT convention.
    let mut qbytes = Vec::with_capacity(query.len() * 4);
    for f in query {
        qbytes.extend_from_slice(&f.to_le_bytes());
    }

    // Moon's FT.SEARCH KNN dialect requires the query to carry the full
    // `<filter>=>[KNN <k> @<field> $<param>]` form — Phase 1.5 used the SDK's
    // `search_raw` with bare filter, which Moon rejects with "invalid KNN
    // query syntax". The SDK's higher-level `search_opts` adds the KNN
    // wrapper but doesn't accept a filter expression. Compose the wrapper
    // here so the filter algebra still works. Live-measurement gap fix
    // 2026-04-21; grammar corrected per contract v1.1 (see module rustdoc).
    let Some(f) = filter else {
        let knn_query = format!("*=>[KNN {k} @vec $query]");
        let reply = search_raw(c, &per_scope_index, &knn_query, &qbytes, k, rerank, as_of).await?;
        return parse_ft_search(reply, rerank, &per_scope_index);
    };

    // Server-side enforcement — only the chunks index declares the TAG /
    // NUMERIC fields the inline grammar can resolve.
    if index == "chunks"
        && let Some(expr) = render_knn_filter(f)
    {
        let knn_query = format!("{expr}=>[KNN {k} @vec $query]");
        let reply = search_raw(c, &per_scope_index, &knn_query, &qbytes, k, rerank, as_of).await?;
        return parse_ft_search(reply, rerank, &per_scope_index);
    }

    // Post-filter path: over-fetch, evaluate against hit metadata, truncate.
    // Recall (not correctness) degrades if >k*4 nearer-neighbours fail the
    // filter — the documented v1.1 degradation.
    let k_fetch = k.saturating_mul(4).clamp(k, 1000);
    tracing::debug!(
        index,
        k,
        k_fetch,
        "vector_search: filter not server-renderable on Moon — over-fetch + client-side post-filter"
    );
    let knn_query = format!("*=>[KNN {k_fetch} @vec $query]");
    let reply =
        search_raw(c, &per_scope_index, &knn_query, &qbytes, k_fetch, rerank, as_of).await?;
    let hits = parse_ft_search(reply, rerank, &per_scope_index)?;
    // Moon's FT.SEARCH reply carries only score fields, not the stored hash —
    // read each candidate's `meta` field lazily (in rank order, stopping at
    // k matches) so the evaluator sees the same metadata `atomic_write` stored.
    let mut typed = c.typed();
    let mut out = Vec::with_capacity(k);
    for mut h in hits {
        if h.metadata.is_null() {
            let key = format!("{per_scope_index}:{}", hex::encode(&h.id));
            let raw: Option<Vec<u8>> =
                typed.hget(key.as_bytes(), "meta").await.map_err(moon_err)?;
            if let Some(bytes) = raw {
                h.metadata = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            }
        }
        if filter_matches(f, &h.metadata)? {
            out.push(h);
            if out.len() == k {
                break;
            }
        }
    }
    Ok(out)
}

async fn search_raw(
    c: &MoonClient,
    index: &str,
    query: &str,
    query_bytes: &[u8],
    k: usize,
    rerank: bool,
    as_of: Option<Hlc>,
) -> Result<redis::Value, StorageError> {
    if as_of.is_none() {
        let typed = c.typed();
        return typed
            .vector()
            .search_raw(index, query, query_bytes, k, rerank)
            .await
            .map_err(moon_err);
    }

    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();
    let rerank_arg = if rerank { "RERANK" } else { "NORERANK" };
    let mut cmd = redis::cmd("FT.SEARCH");
    cmd.arg(index).arg(query).arg("PARAMS").arg(2).arg("query").arg(query_bytes).arg(rerank_arg);
    if let Some(ts) = as_of {
        cmd.arg("AS_OF").arg(ts.wall_ms as i64);
    }
    cmd.arg("LIMIT").arg(0).arg(k);
    cmd.query_async(raw_conn).await.map_err(redis_err)
}

/// Decode a Moon FT.SEARCH result key from `{ft_index}:{hex}` to raw id bytes.
///
/// RFC 0001 Wave 1C: `ft_index` is now the per-scope index name
/// (`lunaris_{scope}_{kind}_idx`) rather than the bare `kind` name. The decode
/// contract is identical — strip the `<ft_index>:` prefix and hex-decode the
/// remainder. Any key that doesn't match the shape is dropped.
///
/// Production chunk ids are ULID bytes, but `StoragePort::vector_search`
/// accepts arbitrary `Vec<u8>` ids and conformance exercises that contract.
pub(crate) fn decode_key(key: &[u8], ft_index: &str) -> Option<Vec<u8>> {
    let prefix_len = ft_index.len() + 1; // +1 for the ':' separator
    if key.len() < prefix_len
        || !key.starts_with(ft_index.as_bytes())
        || key[ft_index.len()] != b':'
    {
        return None;
    }
    hex::decode(&key[prefix_len..]).ok()
}

// Removed `pack_hlc` (Gap 8 / live-measurement 2026-04-21): the
// `TEMPORAL.SNAPSHOT_AT` pre-pin path was deleted from vector/keyword/graph/kv
// after Moon proved it has no KV-AS_OF surface. If a real bi-temporal
// command surface lands upstream, restore the helper from git history rather
// than re-derive it.

/// Render a filter into Moon's FT.SEARCH inline grammar, or `None` when it
/// cannot be expressed faithfully (contract v1.1).
///
/// The grammar (vendor/moon ft_search/parse.rs::parse_filter_string) accepts
/// only space-joined `@field:{tag}` / `@field:[min max]` units: implicit AND,
/// no parens, no `|` OR, no prefix-wildcard. TAG bytes are compared RAW —
/// escaping backslashes match nothing. A brace value equal to `true`/`false`
/// parses as BoolEq (not TagEq) and a multi-word value as full-text
/// TextMatch, so both route to the post-filter path instead.
fn render_knn_filter(f: &Filter) -> Option<String> {
    match f {
        Filter::Eq { field, value } if field == "source" => {
            let v = value.as_str()?;
            if v.is_empty()
                || v.contains(['{', '}', ' '])
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("false")
            {
                return None;
            }
            Some(format!("@source:{{{v}}}"))
        }
        Filter::ValidTimeRange { after, before } => {
            // The chunks FT schema declares `valid_time` NUMERIC
            // (SchemaField::Numeric in ensure_indexes). None maps to the
            // -inf / +inf sentinels Moon's f64 parser accepts.
            // `Filter::ValidTimeRange` is documented half-open `[after,
            // before)`, and Moon reads a bare numeric bound as INCLUSIVE.
            // The upper bound is therefore rendered as `hi - 1`, which is
            // EXACT rather than an approximation: `valid_time_ms` is only
            // ever written from `chunk.bt.valid.0.wall_ms`, an integer
            // number of milliseconds, so the closed range `[lo, hi-1]` and
            // the half-open `[lo, hi)` contain precisely the same integers.
            // If that field ever becomes fractional this stops being exact.
            //
            // Moon's grammar DOES have a `(`-prefix for exclusive bounds and
            // it works on plain FT.SEARCH — but NOT here. The KNN prefilter
            // is parsed by a separate, smaller parser
            // (`vendor/moon/src/command/vector_search/ft_search/parse.rs`)
            // whose numeric branch is a bare `parts[1].parse::<f64>().ok()?`.
            // On `"(200"` that returns None for the WHOLE filter, and the
            // caller degrades to an UNFILTERED search rather than erroring —
            // so `(` here does not narrow the range, it silently removes it.
            // Measured: 4 of 4 rows returned. See ledger F26.
            //
            // `saturating_sub` guards `before = 0`, where the window is
            // empty by construction; `[lo, -1]` matches nothing, which is
            // the right answer.
            let lo = after.map_or("-inf".to_string(), |h| h.wall_ms.to_string());
            let hi =
                before.map_or("+inf".to_string(), |h| h.wall_ms.saturating_sub(1).to_string());
            Some(format!("@valid_time:[{lo} {hi}]"))
        }
        Filter::And(xs) if !xs.is_empty() => {
            let parts: Option<Vec<String>> = xs.iter().map(render_knn_filter).collect();
            Some(parts?.join(" "))
        }
        _ => None,
    }
}

/// Client-side filter evaluation against a hit's `meta` JSON — the v1.1
/// post-filter for shapes Moon's inline grammar cannot express. A filter
/// variant this evaluator does not know is a hard error, never a silent
/// drop (the exact failure mode v1.1 exists to eliminate).
fn filter_matches(f: &Filter, meta: &serde_json::Value) -> Result<bool, StorageError> {
    match f {
        Filter::Eq { field, value } => Ok(meta.get(field) == Some(value)),
        Filter::StartsWith { field, prefix } => Ok(meta
            .get(field)
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with(prefix.as_str()))),
        Filter::And(xs) => {
            for x in xs {
                if !filter_matches(x, meta)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Filter::Or(xs) => {
            for x in xs {
                if filter_matches(x, meta)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Filter::ValidTimeRange { after, before } => {
            let Some(ms) = meta.get("valid_time_ms").and_then(|v| v.as_u64()) else {
                return Ok(false);
            };
            // Half-open `[after, before)` — `<`, not `<=`. This is the
            // client-side twin of the `(`-prefix in `render_knn_filter`, and
            // the two MUST agree: a hit's membership cannot depend on
            // whether Moon evaluated the filter or we did (F21).
            Ok(after.is_none_or(|a| ms >= a.wall_ms) && before.is_none_or(|b| ms < b.wall_ms))
        }
        other => Err(StorageError::Backend(format!(
            "filter_unsupported_on_moon: no post-filter evaluation for {other:?}"
        ))),
    }
}

fn parse_ft_search(
    v: redis::Value,
    rerank: bool,
    ft_index: &str,
) -> Result<Vec<VectorHit>, StorageError> {
    // FT.SEARCH reply: [count, id1, [k1,v1,...], id2, [k1,v1,...], ...]
    let arr = match v {
        redis::Value::Array(a) => a,
        other => {
            return Err(StorageError::Backend(format!("FT.SEARCH unexpected reply: {other:?}")));
        }
    };
    let mut hits = Vec::new();
    let mut iter = arr.into_iter();
    let _count = iter.next();
    while let (Some(id), Some(fields)) = (iter.next(), iter.next()) {
        let raw_key = match id {
            redis::Value::BulkString(b) => b,
            redis::Value::SimpleString(s) => s.into_bytes(),
            _ => continue,
        };
        let id_bytes = match decode_key(&raw_key, ft_index) {
            Some(b) => b,
            None => continue,
        };
        let mut score = 0.0f32;
        // Rerank `__score` outranks the KNN `__vec_score` when both fields
        // appear in one reply, independent of field order.
        let mut rerank_score_seen = false;
        let mut metadata = serde_json::Value::Null;
        if let redis::Value::Array(kv) = fields {
            let mut it = kv.into_iter();
            while let (Some(k), Some(val)) = (it.next(), it.next()) {
                let key = match k {
                    redis::Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
                    redis::Value::SimpleString(s) => s,
                    _ => continue,
                };
                match (key.as_str(), val) {
                    // Moon's KNN `__vec_score` is a DISTANCE (lower = closer;
                    // the server falls back to f32::MAX when missing). The
                    // StoragePort contract is HIGHER = more similar — every
                    // non-Moon backend and every client-side consumer
                    // (`client_side_rrf_weighted` sorts buckets DESCENDING)
                    // assumes it. Convert via the monotone-decreasing
                    // 1/(1+d) ∈ (0,1]; unparseable → 0.0 (worst).
                    ("__vec_score" | "vec_score", redis::Value::BulkString(b)) => {
                        if !rerank_score_seen {
                            score = std::str::from_utf8(&b)
                                .ok()
                                .and_then(|s| s.parse::<f32>().ok())
                                .map(|d| 1.0 / (1.0 + d.max(0.0)))
                                .unwrap_or(0.0);
                        }
                    }
                    // Native-rerank `__score` is a cross-encoder sigmoid —
                    // already higher-is-better; passthrough.
                    ("__score", redis::Value::BulkString(b)) => {
                        rerank_score_seen = true;
                        score = std::str::from_utf8(&b)
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0.0);
                    }
                    ("__metadata" | "meta", redis::Value::BulkString(b)) => {
                        metadata = serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null);
                    }
                    _ => {}
                }
            }
        }
        hits.push(VectorHit { id: id_bytes, score, rerank_applied: rerank, metadata });
    }
    Ok(hits)
}

// ── moon-v051-perf-exploit W1-3: post-bulk-ingest FT.COMPACT maintenance ──

/// Resolve the `LUNARIS_MOON_COMPACT_MIN` gate (whole vector-upsert count).
/// Unset → 512 default, silently; unparseable or `0` → 512 default with a
/// `warn!`. Never returns 0 — a 0 gate would force-compact on every single
/// upsert, defeating the point of batching. Mirrors `parse_op_timeout`.
fn compact_min_threshold() -> usize {
    parse_compact_min(std::env::var("LUNARIS_MOON_COMPACT_MIN").ok().as_deref())
}

/// Pure parser for [`compact_min_threshold`] — split out so the edge cases
/// are unit-testable without mutating the process environment (same
/// rationale as `client::parse_op_timeout`).
fn parse_compact_min(raw: Option<&str>) -> usize {
    const DEFAULT: usize = 512;
    match raw {
        None => DEFAULT,
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!(
                    value = %raw,
                    "ignoring invalid LUNARIS_MOON_COMPACT_MIN (want a positive whole number); \
                     using {DEFAULT} default"
                );
                DEFAULT
            }
        },
    }
}

/// Backing implementation for `StoragePort::maintenance_hint`'s Moon
/// override (`MaintenanceHint::BulkIngestComplete`) — see
/// `lunaris_core::storage::port::MaintenanceHint` for the frozen contract
/// this answers.
///
/// `pub` (not `pub(crate)`, unlike the rest of this file's helpers):
/// `lunaris-storage-moon/src/lib.rs` — the file that owns
/// `impl StoragePort for MoonStorage` — is OUTSIDE this workstream's file
/// ownership (moon-v051-perf-exploit W1 owns `client.rs`/`vector.rs`/
/// `lunaris-core::storage::port`, not `lib.rs`), so this function is
/// exposed at the crate's public surface specifically so:
/// 1. this workstream's own live test can call it directly, and
/// 2. wiring it into the trait is a single-line addition for whoever
///    edits `lib.rs`'s `impl StoragePort for MoonStorage` block:
///    ```ignore
///    async fn maintenance_hint(&self, scope: &Scope, hint: MaintenanceHint) -> Result<(), StorageError> {
///        match hint {
///            MaintenanceHint::BulkIngestComplete { vector_upserts } => {
///                crate::vector::maybe_compact_after_bulk_ingest(&self.client, scope, vector_upserts).await
///            }
///        }
///    }
///    ```
///
/// Below `LUNARIS_MOON_COMPACT_MIN` (default 512) vector upserts, this is a
/// no-op — `Ok(())` without any Moon round trip. At or above the gate, it
/// issues `FT.COMPACT` on all four of the scope's vector indexes
/// (`chunks`/`entities`/`facts`/`communities`) so subsequent recall hits the
/// compacted HNSW + exact-rerank segment instead of the brute-force mutable
/// scan. A missing index (`"Unknown Index name"` — a kind the scope never
/// wrote to) is tolerated, not an error; any other Moon failure propagates
/// (maintenance failures are non-fatal for the CALLER per the trait's doc,
/// but this function still reports them so the caller can log/observe).
pub async fn maybe_compact_after_bulk_ingest(
    client: &MoonClient,
    scope: &Scope,
    vector_upserts: usize,
) -> Result<(), StorageError> {
    maybe_compact_after_bulk_ingest_with_min(client, scope, vector_upserts, compact_min_threshold())
        .await
}

/// [`maybe_compact_after_bulk_ingest`] with the gate passed in explicitly
/// instead of read from `LUNARIS_MOON_COMPACT_MIN`.
///
/// This exists because a test that wants to exercise the at-or-above-gate
/// branch cheaply has to lower the gate, and the only other way to do that is
/// `std::env::set_var` — which is process-wide. `tests/a_maintenance_compact.rs`
/// did exactly that and raced against its own sibling: `§2` lowered the gate to
/// 5 while `§1` was concurrently asserting that 20 upserts stay BELOW the
/// default 512, so `§1` compacted and failed. The test's comment reasoned that
/// grep showed no other site "touching" the variable, but `§1` reads it the way
/// every caller does — indirectly, through this function — so grep could never
/// have found it. Threading the value through as an argument removes the shared
/// mutable state rather than trying to schedule around it.
pub async fn maybe_compact_after_bulk_ingest_with_min(
    client: &MoonClient,
    scope: &Scope,
    vector_upserts: usize,
    compact_min: usize,
) -> Result<(), StorageError> {
    if vector_upserts < compact_min {
        return Ok(());
    }
    let typed = client.typed();
    for kind in SCOPE_VECTOR_INDEX_KINDS {
        let idx = ft_index_name(scope, kind);
        let t = typed.clone();
        if let Err(e) = t.vector().compact(&idx).await {
            let msg = e.to_string();
            if msg.contains("Unknown Index name") {
                continue;
            }
            return Err(moon_err(e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::storage::types::Filter;
    use serde_json::json;

    // ── W1-3: LUNARIS_MOON_COMPACT_MIN parsing ──

    #[test]
    fn parse_compact_min_unset_defaults_to_512() {
        assert_eq!(parse_compact_min(None), 512);
    }

    #[test]
    fn parse_compact_min_honors_valid_override() {
        assert_eq!(parse_compact_min(Some("100")), 100);
        assert_eq!(parse_compact_min(Some("1")), 1);
    }

    #[test]
    fn parse_compact_min_rejects_zero_falls_back_to_default() {
        assert_eq!(parse_compact_min(Some("0")), 512);
    }

    #[test]
    fn parse_compact_min_rejects_garbage_falls_back_to_default() {
        assert_eq!(parse_compact_min(Some("abc")), 512);
        assert_eq!(parse_compact_min(Some("-5")), 512);
        assert_eq!(parse_compact_min(Some("")), 512);
    }

    /// Helper: construct a per-scope FT index name for tests — mirrors what
    /// `atomic.rs` writes and what `vector_search` queries.
    fn test_ft_index(scope_str: &str, kind: &str) -> String {
        let scope = lunaris_core::Scope::new(scope_str).unwrap();
        ft_index_name(&scope, kind)
    }

    // ── ft-navigate-filter-gap contract v1.1 — render_knn_filter pins ──
    // Moon's parse_filter_string compares TAG bytes RAW and aborts on a
    // leading '(' — the renderer must emit unescaped, unparenthesized units.

    #[test]
    fn render_source_tag_raw_no_parens_no_escaping() {
        let f = Filter::Eq { field: "source".into(), value: json!("notes.md") };
        assert_eq!(render_knn_filter(&f).as_deref(), Some("@source:{notes.md}"));
        let f = Filter::Eq { field: "source".into(), value: json!("helios:fs/test") };
        assert_eq!(render_knn_filter(&f).as_deref(), Some("@source:{helios:fs/test}"));
    }

    #[test]
    fn render_rejects_values_the_grammar_misparses() {
        // multi-word -> TextMatch semantics; braces -> parse ambiguity;
        // true/false -> BoolEq. All must fall back to the post-filter path.
        for v in ["two words", "br{ace", "br}ace", "true", "FALSE", ""] {
            let f = Filter::Eq { field: "source".into(), value: json!(v) };
            assert_eq!(render_knn_filter(&f), None, "value {v:?} must not server-render");
        }
    }

    #[test]
    fn render_non_source_eq_not_renderable() {
        // Only `source` is a TAG field on the chunks index; @kind would be
        // TagEq against a non-existent field (matches nothing = wrong).
        let f = Filter::Eq { field: "kind".into(), value: json!("episode") };
        assert_eq!(render_knn_filter(&f), None);
    }

    #[test]
    fn render_startswith_and_or_not_renderable() {
        let f = Filter::StartsWith { field: "source".into(), prefix: "helios:fs/".into() };
        assert_eq!(render_knn_filter(&f), None, "grammar has no prefix-wildcard");
        let f = Filter::Or(vec![
            Filter::Eq { field: "source".into(), value: json!("a.md") },
            Filter::Eq { field: "source".into(), value: json!("b.md") },
        ]);
        assert_eq!(render_knn_filter(&f), None, "grammar has no OR");
    }

    #[test]
    fn render_and_space_joins_without_parens() {
        let f = Filter::And(vec![
            Filter::Eq { field: "source".into(), value: json!("notes.md") },
            Filter::ValidTimeRange {
                after: Some(Hlc { wall_ms: 100, counter: 0, node_id: 0 }),
                before: Some(Hlc { wall_ms: 200, counter: 0, node_id: 0 }),
            },
        ]);
        assert_eq!(
            render_knn_filter(&f).as_deref(),
            Some("@source:{notes.md} @valid_time:[100 199]")
        );
    }

    #[test]
    fn render_and_with_unrenderable_child_is_none() {
        let f = Filter::And(vec![
            Filter::Eq { field: "source".into(), value: json!("notes.md") },
            Filter::StartsWith { field: "source".into(), prefix: "x".into() },
        ]);
        assert_eq!(render_knn_filter(&f), None, "partial server-side enforcement would leak");
    }

    #[test]
    fn render_valid_time_range_bounds() {
        let f = Filter::ValidTimeRange {
            after: Some(Hlc { wall_ms: 100, counter: 0, node_id: 0 }),
            before: None,
        };
        assert_eq!(render_knn_filter(&f).as_deref(), Some("@valid_time:[100 +inf]"));
        let f = Filter::ValidTimeRange {
            after: None,
            before: Some(Hlc { wall_ms: 200, counter: 0, node_id: 0 }),
        };
        assert_eq!(render_knn_filter(&f).as_deref(), Some("@valid_time:[-inf 199]"));
        let f = Filter::ValidTimeRange { after: None, before: None };
        assert_eq!(render_knn_filter(&f).as_deref(), Some("@valid_time:[-inf +inf]"));
    }

    // ── contract v1.1 — filter_matches (client-side post-filter) pins ──

    #[test]
    fn matches_eq_on_metadata() {
        let meta = json!({"source": "alpha.md", "name": "alpha"});
        let f = Filter::Eq { field: "source".into(), value: json!("alpha.md") };
        assert!(filter_matches(&f, &meta).unwrap());
        let f = Filter::Eq { field: "source".into(), value: json!("beta.md") };
        assert!(!filter_matches(&f, &meta).unwrap());
        let f = Filter::Eq { field: "missing".into(), value: json!("x") };
        assert!(!filter_matches(&f, &meta).unwrap(), "missing field never matches");
    }

    #[test]
    fn matches_startswith_and_or() {
        let meta = json!({"source": "helios:fs/test.rs"});
        let f = Filter::StartsWith { field: "source".into(), prefix: "helios:fs/".into() };
        assert!(filter_matches(&f, &meta).unwrap());
        let f = Filter::Or(vec![
            Filter::Eq { field: "source".into(), value: json!("nope.md") },
            Filter::StartsWith { field: "source".into(), prefix: "helios:".into() },
        ]);
        assert!(filter_matches(&f, &meta).unwrap());
        let f = Filter::And(vec![
            Filter::StartsWith { field: "source".into(), prefix: "helios:".into() },
            Filter::Eq { field: "source".into(), value: json!("nope.md") },
        ]);
        assert!(!filter_matches(&f, &meta).unwrap());
    }

    #[test]
    fn matches_valid_time_range_on_metadata() {
        let meta = json!({"valid_time_ms": 150});
        let f = Filter::ValidTimeRange {
            after: Some(Hlc { wall_ms: 100, counter: 0, node_id: 0 }),
            before: Some(Hlc { wall_ms: 200, counter: 0, node_id: 0 }),
        };
        assert!(filter_matches(&f, &meta).unwrap());
        let f = Filter::ValidTimeRange {
            after: Some(Hlc { wall_ms: 151, counter: 0, node_id: 0 }),
            before: None,
        };
        assert!(!filter_matches(&f, &meta).unwrap());
        assert!(
            !filter_matches(
                &Filter::ValidTimeRange { after: None, before: None },
                &json!({"other": 1})
            )
            .unwrap(),
            "no valid_time_ms in metadata -> conservatively excluded"
        );
    }

    /// KG-RAG score-direction fix (2026-07-22): Moon's `__vec_score` is a
    /// DISTANCE (lower = closer; see vendor/moon ft_search/response.rs —
    /// missing score falls back to f32::MAX i.e. worst). The StoragePort
    /// contract is HIGHER = more similar (what every non-Moon backend and
    /// every client-side consumer assumes — `client_side_rrf_weighted` sorts
    /// buckets DESCENDING). Raw passthrough inverted the vector leg's ranking
    /// in any client-side RRF fusion over Moon and collapsed LME evidence
    /// recall to 0% on the fused hybrid root. The adapter must convert
    /// distances to a monotone-decreasing similarity; the native-rerank
    /// `__score` field (sigmoid, already higher-better) stays passthrough.
    #[test]
    fn parse_ft_search_vec_score_distance_converts_to_higher_is_better() {
        let scope = lunaris_core::Scope::new("acme.agent-1").unwrap();
        let ft_idx = ft_index_name(&scope, "chunks");
        let near: [u8; 16] = [1; 16];
        let far: [u8; 16] = [2; 16];
        let row = |id: &[u8; 16], d: &str| {
            vec![
                redis::Value::BulkString(format!("{ft_idx}:{}", hex::encode(id)).into_bytes()),
                redis::Value::Array(vec![
                    redis::Value::BulkString(b"__vec_score".to_vec()),
                    redis::Value::BulkString(d.as_bytes().to_vec()),
                ]),
            ]
        };
        let mut arr = vec![redis::Value::Int(2)];
        arr.extend(row(&near, "0.10")); // closer neighbour
        arr.extend(row(&far, "0.50")); // farther neighbour
        let hits = parse_ft_search(redis::Value::Array(arr), false, &ft_idx).unwrap();

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, near.to_vec());
        assert!(
            hits[0].score > hits[1].score,
            "distance 0.10 must map to a HIGHER score than distance 0.50 \
             (StoragePort contract: higher = more similar); got {} vs {}",
            hits[0].score,
            hits[1].score
        );
        assert!(
            hits.iter().all(|h| h.score.is_finite() && h.score > 0.0 && h.score <= 1.0),
            "converted similarity must live in (0, 1]; got {hits:?}"
        );
    }

    #[test]
    fn parse_ft_search_empty_returns_empty_vec() {
        let ft_idx = test_ft_index("_dev_", "chunks");
        let reply = redis::Value::Array(vec![redis::Value::Int(0)]);
        let hits = parse_ft_search(reply, false, &ft_idx).unwrap();
        assert!(hits.is_empty());
    }

    /// RFC 0001 Wave 1C: FT.SEARCH keys are now `{ft_index_name(scope,kind)}:{hex32}`.
    /// `decode_key` must strip the per-scope prefix correctly to recover 16-byte ULID.
    #[test]
    fn parse_ft_search_decodes_per_scope_index_prefixed_hex_keys() {
        let scope = lunaris_core::Scope::new("acme.agent-1").unwrap();
        let ft_idx = ft_index_name(&scope, "chunks");
        let ulid_bytes: [u8; 16] =
            [1, 157, 184, 227, 97, 47, 203, 75, 14, 1, 202, 211, 92, 115, 47, 72];
        let hex = hex::encode(ulid_bytes);
        let key = format!("{ft_idx}:{hex}");
        let reply = redis::Value::Array(vec![
            redis::Value::Int(1),
            redis::Value::BulkString(key.into_bytes()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"__score".to_vec()),
                redis::Value::BulkString(b"0.91".to_vec()),
            ]),
        ]);
        let hits = parse_ft_search(reply, true, &ft_idx).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, ulid_bytes.to_vec());
        assert!((hits[0].score - 0.91).abs() < 1e-4);
        assert!(hits[0].rerank_applied);
    }

    #[test]
    fn parse_ft_search_drops_malformed_keys() {
        let ft_idx = test_ft_index("_dev_", "chunks");
        let reply = redis::Value::Array(vec![
            redis::Value::Int(1),
            redis::Value::BulkString(b"wrongprefix:abc".to_vec()),
            redis::Value::Array(vec![]),
        ]);
        let hits = parse_ft_search(reply, false, &ft_idx).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn decode_key_round_trip_with_scope() {
        let scope = lunaris_core::Scope::new("acme.agent-1").unwrap();
        let ft_idx = ft_index_name(&scope, "chunks");
        let ulid: [u8; 16] = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let key = format!("{ft_idx}:{}", hex::encode(ulid));
        assert_eq!(decode_key(key.as_bytes(), &ft_idx), Some(ulid.to_vec()));
        // A key from a different scope's index must not decode against scope_a's index.
        let scope_b = lunaris_core::Scope::new("other.agent-2").unwrap();
        let ft_idx_b = ft_index_name(&scope_b, "chunks");
        assert_eq!(decode_key(key.as_bytes(), &ft_idx_b), None);
    }

    #[test]
    fn decode_key_accepts_non_ulid_ids() {
        let ft_idx = test_ft_index("_dev_", "chunks");
        let id = b"chunk-conformance-000";
        let key = format!("{ft_idx}:{}", hex::encode(id));
        assert_eq!(decode_key(key.as_bytes(), &ft_idx), Some(id.to_vec()));
    }

    #[test]
    fn decode_key_rejects_wrong_prefix() {
        let ft_idx = test_ft_index("_dev_", "chunks");
        assert_eq!(decode_key(b"facts:00", ft_idx.as_str()), None);
        assert_eq!(decode_key(format!("{ft_idx}:notHex!").as_bytes(), ft_idx.as_str()), None);
    }
}
