//! `keyword_search` — Moon BM25 via `FT.SEARCH`.
//!
//! RFC 0001 Wave 1C: the FT index consulted is the per-scope index
//! `ft_index_name(scope, index)` so keyword searches are scoped-isolated.
//! The key-decode path mirrors `vector::decode_key` — strip `<ft_idx>:` prefix
//! and hex-decode to 16-byte ULID.
//!
//! Wire shape: the SDK issues
//!
//! ```text
//! FT.SEARCH <index> "<escaped_query>" LIMIT 0 <k> [RETURN ...]
//! ```
//!
//! against the Moon `FT.*` module. Moon's default scorer for `FT.SEARCH` is BM25
//! (per Moon's RediSearch surface), and the typed SDK returns
//! `Vec<TextSearchHit { key, score, fields }>`. We re-shape that into the Lunaris
//! [`KeywordHit`] contract: bytes id + per-call min-max normalized score + raw
//! score + JSON metadata.
//!
//! ## AS_OF semantics
//!
//! When `as_of = Some(t)`, Lunaris emits Moon's native
//! `FT.SEARCH ... AS_OF <t.wall_ms>` clause on the search command. We do not
//! pre-pin the connection with `TEMPORAL.SNAPSHOT_AT`; Moon's FT parser owns the
//! temporal lookup for keyword search.
//!
//! ## Filter algebra
//!
//! `Filter::Eq` / `Filter::StartsWith` / `Filter::And` / `Filter::Or` translate to
//! Moon's FT query DSL the same way as `vector::filter_to_moon`: `@field:value`,
//! `@field:prefix*`, space-joined for AND, `|` for OR. Unlike `vector_search`
//! which has a separate filter slot, FT.SEARCH wants ONE composite query
//! string — we render `(<filter>) <query>` so the filter narrows the candidate
//! set before BM25 ranks it.
//!
//! ## Threat model — T-02-02-02
//!
//! Moon FT query syntax has its own escape rules: `-`, `(`, `)`, `:`, `|`, `~`,
//! `\` etc. all carry meaning. Raw user input flowing into the query string can
//! break the parser OR worse — flip the semantics by injecting boolean ops.
//! [`ft_escape`] backslash-escapes every special character per the RediSearch FT
//! spec before the query reaches the wire.

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::keyword::{KeywordHit, min_max_normalize};
use lunaris_core::storage::types::Filter;

use crate::client::{MoonClient, moon_err, redis_err};
use crate::keyspace::ft_index_name;

#[derive(Debug)]
struct TextSearchHit {
    key: String,
    score: f64,
    fields: std::collections::HashMap<String, String>,
}

pub(crate) async fn keyword_search(
    c: &MoonClient,
    scope: &Scope,
    index: &str,
    query: &str,
    k: usize,
    filter: Option<&Filter>,
    as_of: Option<Hlc>,
) -> Result<Vec<KeywordHit>, StorageError> {
    // T-02-02-01 mitigation: whitelist match on `index`. Anything else returns
    // `StorageError::Backend`.
    match index {
        "chunks" | "entities" | "facts" | "communities" => {}
        other => {
            return Err(StorageError::Backend(format!(
                "unknown keyword index: {other} (valid: chunks|entities|facts|communities)"
            )));
        }
    }

    // RFC 0001 Wave 1C: route to per-scope FT index.
    let per_scope_index = ft_index_name(scope, index);

    let query_escaped = ft_escape(query);
    let composite = match filter {
        Some(f) => format!("({}) {}", filter_to_moon(f), query_escaped),
        None => query_escaped,
    };

    // Moon's FT.SEARCH default scorer is BM25 (per RediSearch behavior).
    let hits = search_text(c, &per_scope_index, &composite, k, as_of).await?;

    // Stage raw scores so we can min-max normalize per the KeywordPort contract.
    let mut staged: Vec<(Vec<u8>, serde_json::Value, f32)> = Vec::with_capacity(hits.len());
    for h in hits {
        // Moon returns keys as `{ft_index_name(scope,kind)}:{hex32}` (per
        // atomic.rs::VectorUpsert). Symmetric with vector::decode_key —
        // hydrate expects 16-byte ULID.
        let raw_key = h.key.into_bytes();
        let id_bytes = match decode_moon_key(&raw_key, &per_scope_index) {
            Some(b) => b,
            None => continue,
        };
        // Recover __metadata if Moon stored it as a field; otherwise keep null.
        let metadata = h
            .fields
            .get("__metadata")
            .or_else(|| h.fields.get("meta"))
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .unwrap_or(serde_json::Value::Null);
        staged.push((id_bytes, metadata, h.score as f32));
    }

    let raw_scores: Vec<f32> = staged.iter().map(|(_, _, r)| *r).collect();
    let normalized = min_max_normalize(&raw_scores);

    Ok(staged
        .into_iter()
        .zip(normalized)
        .map(|((id, metadata, raw), score)| KeywordHit { id, score, raw_score: raw, metadata })
        .collect())
}

async fn search_text(
    c: &MoonClient,
    index: &str,
    query: &str,
    k: usize,
    as_of: Option<Hlc>,
) -> Result<Vec<TextSearchHit>, StorageError> {
    if as_of.is_none() {
        let typed = c.typed();
        let mut text = typed.text();
        return text
            .search(index, query, k, None)
            .await
            .map(|hits| {
                hits.into_iter()
                    .map(|h| TextSearchHit { key: h.key, score: h.score, fields: h.fields })
                    .collect()
            })
            .map_err(moon_err);
    }

    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();
    let mut cmd = redis::cmd("FT.SEARCH");
    cmd.arg(index).arg(query);
    if let Some(ts) = as_of {
        cmd.arg("AS_OF").arg(ts.wall_ms as i64);
    }
    cmd.arg("LIMIT").arg(0).arg(k);

    let raw = cmd.query_async(raw_conn).await.map_err(redis_err)?;
    Ok(parse_text_search_hits(raw))
}

fn parse_text_search_hits(raw: redis::Value) -> Vec<TextSearchHit> {
    let arr = match raw {
        redis::Value::Array(a) => a,
        _ => return vec![],
    };
    let mut hits = Vec::new();
    let mut iter = arr.into_iter();
    let _count = iter.next();

    while let (Some(key), Some(fields)) = (iter.next(), iter.next()) {
        let key = value_to_string(key);
        let mut score = 0.0_f64;
        let mut parsed_fields = std::collections::HashMap::new();

        if let redis::Value::Array(kv) = fields {
            let mut it = kv.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                let field = value_to_string(k);
                let value = value_to_string(v);
                match field.as_str() {
                    "__vec_score" | "vec_score" | "__score" | "score" => {
                        score = value.parse().unwrap_or(0.0);
                    }
                    _ => {
                        parsed_fields.insert(field, value);
                    }
                }
            }
        }

        hits.push(TextSearchHit { key, score, fields: parsed_fields });
    }

    hits
}

fn value_to_string(value: redis::Value) -> String {
    match value {
        redis::Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
        redis::Value::SimpleString(s) => s,
        redis::Value::Int(i) => i.to_string(),
        redis::Value::Double(f) => f.to_string(),
        redis::Value::Boolean(b) => b.to_string(),
        redis::Value::Nil => String::new(),
        other => format!("{other:?}"),
    }
}

/// Render a [`Filter`] tree as a Moon FT query expression.
///
/// Mirrors `vector::filter_to_moon` byte-for-byte. Re-implemented locally
/// (small helper) instead of extracted to avoid a public re-export between
/// per-method modules — they share the algebra but each module owns the wire
/// shape it composes.
fn filter_to_moon(f: &Filter) -> String {
    match f {
        Filter::Eq { field, value } => {
            if field == "source" {
                // source is a TAG field on the chunks FT index (PERF-MOON-01).
                // TAG syntax uses `{value}` braces. Special characters inside
                // the value must be backslash-escaped per RediSearch TAG rules.
                format!("@{field}:{{{}}}", ft_tag_escape(&json_to_moon_bare(value)))
            } else {
                format!("@{field}:{}", json_to_moon(value))
            }
        }
        Filter::StartsWith { field, prefix } => format!("@{field}:{prefix}*"),
        Filter::And(xs) => {
            format!("({})", xs.iter().map(filter_to_moon).collect::<Vec<_>>().join(" "))
        }
        Filter::Or(xs) => {
            format!("({})", xs.iter().map(filter_to_moon).collect::<Vec<_>>().join(" | "))
        }
        Filter::ValidTimeRange { after, before } => {
            // RediSearch numeric range on `@valid_time`. Inline string render —
            // no hlc_to_moon_score helper exists (verified 2026-04-22). None
            // maps to -inf / +inf sentinels. The Moon FT schema on the `chunks`
            // index declares `valid_time` NUMERIC (Plan 09.1-02 Task 2 landed
            // SchemaField::Numeric("valid_time") in ensure_indexes).
            let lo = after.map_or("-inf".to_string(), |h| h.wall_ms.to_string());
            let hi = before.map_or("+inf".to_string(), |h| h.wall_ms.to_string());
            format!("@valid_time:[{lo} {hi}]")
        }
        // `#[non_exhaustive]` on `Filter` requires a wildcard arm. New
        // variants need explicit Moon-side rendering; until that lands,
        // skip the predicate (matches everything) and warn — never
        // silently misrepresent the filter.
        _ => {
            tracing::warn!(
                "unknown Filter variant in Moon keyword path — emitting unconstrained predicate"
            );
            "*".to_string()
        }
    }
}

/// Strip `{ft_index}:` prefix and hex-decode the rest to 16-byte ULID.
///
/// RFC 0001 Wave 1C: `ft_index` is now the per-scope index name
/// (`lunaris_{scope}_{kind}_idx`). The 16-byte ULID decode contract is preserved —
/// only the prefix length changes. Mirrors `vector::decode_key`.
fn decode_moon_key(key: &[u8], ft_index: &str) -> Option<Vec<u8>> {
    let prefix_len = ft_index.len() + 1; // +1 for ':'
    if key.len() < prefix_len
        || !key.starts_with(ft_index.as_bytes())
        || key[ft_index.len()] != b':'
    {
        return None;
    }
    hex::decode(&key[prefix_len..]).ok().filter(|b| b.len() == 16)
}

/// Return the bare string value WITHOUT quotes — TAG values must not be quoted.
fn json_to_moon_bare(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => format!("{v}"),
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

fn json_to_moon(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => format!("\"{v}\""),
    }
}

/// Escape FT special characters per the RediSearch syntax spec.
///
/// FT specials per `https://redis.io/docs/latest/develop/interact/search-and-query/`:
/// `,`, `.`, `<`, `>`, `{`, `}`, `[`, `]`, `"`, `'`, `:`, `;`, `!`, `@`, `#`,
/// `$`, `%`, `^`, `&`, `*`, `(`, `)`, `-`, `+`, `=`, `~`, `|`, `\`, ` ` (space
/// inside a token), `/`, `?`. Each becomes `\X` so the FT parser treats the
/// rune as a literal.
///
/// T-02-02-02 mitigation: prevents `"foo (bar)"` style queries from breaking the
/// parser AND prevents `"foo|bar"` from silently widening the boolean.
pub fn ft_escape(s: &str) -> String {
    const SPECIALS: &[char] = &[
        ',', '.', '<', '>', '{', '}', '[', ']', '"', '\'', ':', ';', '!', '@', '#', '$', '%', '^',
        '&', '*', '(', ')', '-', '+', '=', '~', '|', '\\', '/', '?',
    ];
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        if SPECIALS.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build one `[key, [field, value, …]]` FT.SEARCH row.
    fn ft_row(key: &str, fields: &[(&str, &str)]) -> Vec<redis::Value> {
        let mut kv = Vec::with_capacity(fields.len() * 2);
        for (f, v) in fields {
            kv.push(redis::Value::BulkString(f.as_bytes().to_vec()));
            kv.push(redis::Value::BulkString(v.as_bytes().to_vec()));
        }
        vec![redis::Value::BulkString(key.as_bytes().to_vec()), redis::Value::Array(kv)]
    }

    fn ft_reply(rows: Vec<Vec<redis::Value>>) -> redis::Value {
        let mut arr = vec![redis::Value::Int(rows.len() as i64)];
        for r in rows {
            arr.extend(r);
        }
        redis::Value::Array(arr)
    }

    /// Score-direction fix, keyword leg (0.6.1): the sibling of the
    /// `vector.rs::parse_ft_search` fix. Moon's `__vec_score` is a DISTANCE
    /// (lower = closer); `TextSearchHit.score` feeds `min_max_normalize`
    /// straight into `KeywordHit { score, raw_score }`, whose contract —
    /// like every other StoragePort score — is HIGHER = more similar.
    /// Raw passthrough therefore inverts the ranking of any FT.SEARCH reply
    /// that carries a KNN distance field: the FARTHEST hit normalizes to 1.0.
    /// Invariant: lower distance MUST map to a strictly higher score.
    #[test]
    fn parse_text_search_hits_vec_score_distance_converts_to_higher_is_better() {
        for field in ["__vec_score", "vec_score"] {
            let raw = ft_reply(vec![
                ft_row("idx:aa", &[(field, "0.10")]), // closer neighbour
                ft_row("idx:bb", &[(field, "0.90")]), // farther neighbour
            ]);
            let hits = parse_text_search_hits(raw);

            assert_eq!(hits.len(), 2, "{field}");
            assert!(
                hits[0].score > hits[1].score,
                "{field}: distance 0.10 must map to a HIGHER score than distance 0.90 \
                 (KeywordHit contract: higher = more similar); got {} vs {}",
                hits[0].score,
                hits[1].score
            );
            assert!(
                hits.iter().all(|h| h.score.is_finite() && h.score > 0.0 && h.score <= 1.0),
                "{field}: converted similarity must live in (0, 1]; got {hits:?}"
            );

            // The harm this prevents, one layer down: `keyword_search` runs the
            // parsed scores through `min_max_normalize`, so an un-inverted
            // distance hands the FARTHEST hit the top normalized score.
            let normalized =
                min_max_normalize(&hits.iter().map(|h| h.score as f32).collect::<Vec<_>>());
            assert!(
                normalized[0] > normalized[1],
                "{field}: after min_max_normalize the CLOSER hit must rank first; got {normalized:?}"
            );
        }
    }

    /// The other half of the same match: BM25 `score` / `__score` are genuine
    /// relevance scores (higher = better) and MUST stay untouched. Converting
    /// them would invert the keyword leg itself.
    #[test]
    fn parse_text_search_hits_bm25_score_stays_passthrough() {
        for field in ["__score", "score"] {
            let raw = ft_reply(vec![
                ft_row("idx:aa", &[(field, "4.25")]),
                ft_row("idx:bb", &[(field, "2.10")]),
            ]);
            let hits = parse_text_search_hits(raw);

            assert_eq!(hits.len(), 2, "{field}");
            assert!(
                (hits[0].score - 4.25).abs() < 1e-9 && (hits[1].score - 2.10).abs() < 1e-9,
                "{field}: BM25 relevance is already higher-is-better — passthrough verbatim, \
                 no 1/(1+d); got {} and {}",
                hits[0].score,
                hits[1].score
            );
        }
    }

    #[test]
    fn ft_escape_passes_alnum() {
        assert_eq!(ft_escape("hello world"), "hello world");
        assert_eq!(ft_escape("foo123"), "foo123");
    }

    #[test]
    fn ft_escape_backslashes_specials() {
        assert_eq!(ft_escape("foo (bar)"), r"foo \(bar\)");
        assert_eq!(ft_escape("a:b"), r"a\:b");
        assert_eq!(ft_escape("a|b"), r"a\|b");
        assert_eq!(ft_escape("foo-bar"), r"foo\-bar");
    }

    #[test]
    fn ft_escape_handles_empty() {
        assert_eq!(ft_escape(""), "");
    }

    /// Plan 15-01 Task 3 — source is a TAG field; TAG syntax uses `{value}`
    /// braces. Dots inside the value must be backslash-escaped per RediSearch
    /// TAG rules.
    #[test]
    fn filter_eq_renders_source_as_tag_keyword() {
        let f = Filter::Eq { field: "source".into(), value: json!("notes.md") };
        assert_eq!(filter_to_moon(&f), "@source:{notes\\.md}");
    }

    #[test]
    fn filter_eq_renders_source_with_colon_as_tag_keyword() {
        let f = Filter::Eq { field: "source".into(), value: json!("helios:fs/test") };
        assert_eq!(filter_to_moon(&f), "@source:{helios\\:fs/test}");
    }

    #[test]
    fn filter_eq_renders_non_source_as_text_keyword() {
        let f = Filter::Eq { field: "kind".into(), value: json!("episode") };
        assert_eq!(filter_to_moon(&f), "@kind:\"episode\"");
    }

    #[test]
    fn filter_starts_with_renders_for_keyword() {
        let f = Filter::StartsWith { field: "source".into(), prefix: "helios:fs/".into() };
        assert_eq!(filter_to_moon(&f), "@source:helios:fs/*");
    }

    // Plan 09.1-02 Task 3 — ValidTimeRange rendering tests (byte-identical
    // output to vector.rs::filter_to_moon; mirrors 09-PATTERNS.md decision
    // #2 probe_backend convention).
    #[test]
    fn filter_to_moon_valid_time_range_both_bounds() {
        let f = Filter::ValidTimeRange {
            after: Some(Hlc { wall_ms: 100, counter: 0, node_id: 0 }),
            before: Some(Hlc { wall_ms: 200, counter: 0, node_id: 0 }),
        };
        assert_eq!(filter_to_moon(&f), "@valid_time:[100 200]");
    }

    #[test]
    fn filter_to_moon_valid_time_range_only_after() {
        let f = Filter::ValidTimeRange {
            after: Some(Hlc { wall_ms: 100, counter: 0, node_id: 0 }),
            before: None,
        };
        assert_eq!(filter_to_moon(&f), "@valid_time:[100 +inf]");
    }

    #[test]
    fn filter_to_moon_valid_time_range_only_before() {
        let f = Filter::ValidTimeRange {
            after: None,
            before: Some(Hlc { wall_ms: 200, counter: 0, node_id: 0 }),
        };
        assert_eq!(filter_to_moon(&f), "@valid_time:[-inf 200]");
    }

    #[test]
    fn filter_to_moon_valid_time_range_both_none() {
        let f = Filter::ValidTimeRange { after: None, before: None };
        assert_eq!(filter_to_moon(&f), "@valid_time:[-inf +inf]");
    }

    /// RFC 0001 Wave 1C — decode_moon_key must use the per-scope FT index name.
    #[test]
    fn decode_moon_key_per_scope_round_trip() {
        let scope = lunaris_core::Scope::new("acme.agent-1").unwrap();
        let ft_idx = ft_index_name(&scope, "chunks");
        let ulid: [u8; 16] = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let key = format!("{ft_idx}:{}", hex::encode(ulid));
        assert_eq!(decode_moon_key(key.as_bytes(), &ft_idx), Some(ulid.to_vec()));

        // Key from a different scope must not decode against scope_a's index.
        let scope_b = lunaris_core::Scope::new("other.agent").unwrap();
        let ft_idx_b = ft_index_name(&scope_b, "chunks");
        assert_eq!(decode_moon_key(key.as_bytes(), &ft_idx_b), None);
    }
}
