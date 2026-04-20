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
use crate::vector::pack_hlc;

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

    if let Some(t) = as_of {
        let pinned = pack_hlc(t);
        typed.temporal().snapshot_at_packed(pinned).await.map_err(moon_err)?;
    }

    let query_escaped = ft_escape(query);
    let composite = match filter {
        Some(f) => format!("({}) {}", filter_to_moon(f), query_escaped),
        None => query_escaped,
    };

    // Moon's FT.SEARCH default scorer is BM25 (per RediSearch behavior).
    // The typed SDK returns Vec<TextSearchHit> with `key`, `score`, `fields`.
    let mut text = typed.text();
    let search_result = text.search(index, &composite, k, None).await;

    if as_of.is_some() {
        // Best effort — release the snapshot pin even if FT.SEARCH errored.
        let _ = typed.temporal().release_snapshot().await;
    }

    let hits = search_result.map_err(moon_err)?;

    // Stage raw scores so we can min-max normalize per the KeywordPort contract.
    let mut staged: Vec<(Vec<u8>, serde_json::Value, f32)> = Vec::with_capacity(hits.len());
    for h in hits {
        let id_bytes = h.key.into_bytes();
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
    }
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
}
