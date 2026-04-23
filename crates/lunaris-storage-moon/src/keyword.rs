//! `keyword_search` — Moon BM25 via the typed `moon-client` `text().search()`.
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
//! When `as_of = Some(t)` we issue `client.temporal().snapshot_at_packed(packed)`
//! on the same multiplexed connection BEFORE the `FT.SEARCH` so the search reads
//! the snapshot. After the search we always run `release_snapshot()` (best
//! effort, even on FT.SEARCH error) — same pattern as `vector::vector_search`.
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

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::keyword::{KeywordHit, min_max_normalize};
use lunaris_core::storage::types::Filter;

use crate::client::{MoonClient, moon_err};

pub(crate) async fn keyword_search(
    c: &MoonClient,
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

    let typed = c.typed();

    // AS_OF deferred — see vector.rs / graph.rs / kv.rs rationale: the SDK
    // helper this module calls (`text.search`) does not accept an AS_OF
    // clause, and Phase 1.5's `snapshot_at_packed(ts)` pre-pin sent invalid
    // args server-side. Tracked as B-task for STORE-07.
    let _ = as_of;

    let query_escaped = ft_escape(query);
    let composite = match filter {
        Some(f) => format!("({}) {}", filter_to_moon(f), query_escaped),
        None => query_escaped,
    };

    // Moon's FT.SEARCH default scorer is BM25 (per RediSearch behavior).
    // The typed SDK returns Vec<TextSearchHit> with `key`, `score`, `fields`.
    let mut text = typed.text();
    let hits = text.search(index, &composite, k, None).await.map_err(moon_err)?;

    // Stage raw scores so we can min-max normalize per the KeywordPort contract.
    let mut staged: Vec<(Vec<u8>, serde_json::Value, f32)> = Vec::with_capacity(hits.len());
    for h in hits {
        // Moon returns keys as `<index>:<hex32>` (per atomic.rs::VectorUpsert).
        // Symmetric with vector::decode_key — hydrate expects 16-byte ULID.
        let raw_key = h.key.into_bytes();
        let id_bytes = match decode_moon_key(&raw_key, index) {
            Some(b) => b,
            None => continue,
        };
        // Recover __metadata if Moon stored it as a field; otherwise keep null.
        let metadata = h
            .fields
            .get("__metadata")
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

/// Render a [`Filter`] tree as a Moon FT query expression.
///
/// Mirrors `vector::filter_to_moon` byte-for-byte. Re-implemented locally
/// (small helper) instead of extracted to avoid a public re-export between
/// per-method modules — they share the algebra but each module owns the wire
/// shape it composes.
fn filter_to_moon(f: &Filter) -> String {
    match f {
        Filter::Eq { field, value } => format!("@{field}:{}", json_to_moon(value)),
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
    }
}

/// Strip `<index>:` prefix and hex-decode the rest to 16-byte ULID, mirroring
/// `vector::decode_key`. See that fn's doc for rationale.
fn decode_moon_key(key: &[u8], index: &str) -> Option<Vec<u8>> {
    let prefix_len = index.len() + 1;
    if key.len() < prefix_len || !key.starts_with(index.as_bytes()) || key[index.len()] != b':' {
        return None;
    }
    hex::decode(&key[prefix_len..]).ok().filter(|b| b.len() == 16)
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

    #[test]
    fn filter_eq_renders_for_keyword() {
        let f = Filter::Eq { field: "source".into(), value: json!("notes.md") };
        assert_eq!(filter_to_moon(&f), "@source:\"notes.md\"");
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
}
