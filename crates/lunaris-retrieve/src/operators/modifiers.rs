//! `top(n)` modifier + the v0 string-DSL `filter_str` parser.
//!
//! `filter` and `as_of` modifiers don't need their own retriever wrappers —
//! they set fields on the `Query` carried by [`super::QueryContext`]. Those
//! setters live on [`crate::builder::RetrievalBuilder`]. Only `top` needs a
//! retriever wrapper because it operates on the post-tree result list.
//!
//! ## `filter_str` v0 grammar
//!
//! Supported (T-02-02-06 mitigation — anything else returns `Err`):
//!
//! - `field = 'value'` → `Filter::Eq { field, value: Value::String(value) }`
//! - `field LIKE 'prefix%'` → `Filter::StartsWith { field, prefix }`
//!   (the `%` MUST be the last character — no embedded `%` allowed)
//!
//! NOT supported in v0 (parser returns `Err`):
//! - Boolean composition: callers using complex predicates construct
//!   `Filter::And` / `Filter::Or` programmatically.
//! - Glob `%` in the middle of a pattern (e.g., `'foo%bar%'`).
//! - LIKE without a trailing `%` (we expose a single semantic: prefix match).

use std::any::Any;

use async_trait::async_trait;
use lunaris_core::LunarisError;
use lunaris_core::storage::types::Filter;
use thiserror::Error;

use super::{QueryContext, Retriever};
use crate::types::RawHit;

/// Cap the result list to `n` hits after the upstream operator resolves.
///
/// Sort by descending score before truncating so callers always get the
/// top-N by score (regardless of how the upstream returned them).
pub struct TopRetriever {
    pub(crate) inner: Box<dyn Retriever>,
    pub(crate) n: usize,
}

impl TopRetriever {
    pub fn new(inner: Box<dyn Retriever>, n: usize) -> Self {
        Self { inner, n }
    }
}

#[async_trait]
impl Retriever for TopRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        let mut hits = self.inner.retrieve(ctx).await?;
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(self.n);
        Ok(hits)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Chunk-superset floored top-k selection (pure function; the policy behind
/// [`FlooredTopRetriever`]).
///
/// LME N=125 A/B diagnosis (2026-07-29, graph-ON 83.2% vs graph-OFF 88.0%):
/// with a plain `top(n)` cut, reranked fact/Navigate hits that outrank
/// mid-value evidence chunks EVICT them from the final context (q251: the
/// date-bearing chunk that was graph-OFF's `hit[1]` at 0.513 vanished from
/// graph-ON's entire list). Because the chunk legs and cross-encoder scores
/// are identical across arms, reserving `floor_n` slots for floor-leg hits
/// makes the graph-ON chunk context a strict superset of graph-OFF's — extra
/// legs can only ADD context, never displace it.
///
/// Selection contract:
/// - Output is at most `n` hits, in descending score order.
/// - At least `min(floor_n, n, available_floor_hits)` of them match the
///   floor: metadata `"index"` equals `floor_index`, or the hit carries NO
///   `"index"` tag at all (untagged hits predate per-leg tagging — they come
///   from chunk-era legs, so they fail open to the floor rather than being
///   silently demoted to headroom candidates).
/// - The remaining slots are filled by global score order regardless of leg.
/// - `hits.len() <= n` degenerates to a plain sort (nothing to displace).
pub fn floored_top(
    mut hits: Vec<RawHit>,
    n: usize,
    floor_index: &str,
    floor_n: usize,
) -> Vec<RawHit> {
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    if hits.len() <= n {
        return hits;
    }
    let is_floor = |h: &RawHit| match h.metadata.get("index") {
        Some(serde_json::Value::String(ix)) => ix == floor_index,
        _ => true,
    };
    let mut selected = vec![false; hits.len()];
    let mut taken = 0usize;
    let floor_target = floor_n.min(n);
    for (i, h) in hits.iter().enumerate() {
        if taken == floor_target {
            break;
        }
        if is_floor(h) {
            selected[i] = true;
            taken += 1;
        }
    }
    for flag in selected.iter_mut() {
        if taken == n {
            break;
        }
        if !*flag {
            *flag = true;
            taken += 1;
        }
    }
    hits.into_iter().zip(selected).filter_map(|(h, keep)| keep.then_some(h)).collect()
}

/// [`TopRetriever`] with a reserved per-leg floor — see [`floored_top`] for
/// the selection contract and the displacement regression it closes.
///
/// GA-1 scope note (2026-08-17): this operator is a **bench/DSL-level
/// primitive only** — its sole consumer is the LongMemEval harness
/// (`lunaris-bench`). It is deliberately NOT part of the unified production
/// root (`crate::composition::production_root`); wiring it into a
/// production surface is a future, separately-validated decision.
pub struct FlooredTopRetriever {
    inner: Box<dyn Retriever>,
    n: usize,
    floor_index: String,
    floor_n: usize,
}

impl FlooredTopRetriever {
    pub fn new(inner: Box<dyn Retriever>, n: usize, floor_index: &str, floor_n: usize) -> Self {
        Self { inner, n, floor_index: floor_index.to_owned(), floor_n }
    }

    pub fn n(&self) -> usize {
        self.n
    }

    pub fn floor_index(&self) -> &str {
        &self.floor_index
    }

    pub fn floor_n(&self) -> usize {
        self.floor_n
    }
}

#[async_trait]
impl Retriever for FlooredTopRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        let hits = self.inner.retrieve(ctx).await?;
        Ok(floored_top(hits, self.n, &self.floor_index, self.floor_n))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// --------------------------------------------------------------------- filter_str

/// Errors from the v0 [`filter_str`] parser. Returned at builder time so
/// callers see the failure before any backend IO.
#[derive(Debug, Error)]
pub enum FilterParseError {
    #[error(
        "filter_str: unsupported syntax — v0 supports only `field = 'value'` and `field LIKE 'prefix%'`"
    )]
    Unsupported,
    #[error("filter_str: missing closing quote")]
    UnclosedQuote,
    #[error("filter_str: LIKE pattern must end with `%` and not contain other `%`")]
    UnsupportedGlob,
}

/// Parse the v0 string DSL into a [`Filter`] AST.
///
/// See module rustdoc for the supported grammar.
pub fn filter_str(s: &str) -> Result<Filter, FilterParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(FilterParseError::Unsupported);
    }

    // Try `field LIKE '...'` first (longer keyword).
    if let Some((field, rest)) = split_keyword_ci(s, " LIKE ") {
        let pattern = read_quoted(rest)?;
        // Must end with `%` and contain NO other `%`.
        let body = pattern.strip_suffix('%').ok_or(FilterParseError::UnsupportedGlob)?;
        if body.contains('%') {
            return Err(FilterParseError::UnsupportedGlob);
        }
        return Ok(Filter::StartsWith {
            field: field.trim().to_string(),
            prefix: body.to_string(),
        });
    }

    // Try `field = '...'` next.
    if let Some((field, rest)) = split_keyword_ci(s, " = ") {
        let value = read_quoted(rest)?;
        return Ok(Filter::Eq {
            field: field.trim().to_string(),
            value: serde_json::Value::String(value.to_string()),
        });
    }

    Err(FilterParseError::Unsupported)
}

/// Split `s` on the first case-sensitive occurrence of `sep` (with surrounding
/// whitespace). Returns `(left, right)` where `right` is past `sep`.
fn split_keyword_ci<'a>(s: &'a str, sep: &str) -> Option<(&'a str, &'a str)> {
    s.find(sep).map(|idx| (&s[..idx], &s[idx + sep.len()..]))
}

/// Strip a single-quoted string literal from the head of `s`.
fn read_quoted(s: &str) -> Result<&str, FilterParseError> {
    let s = s.trim_start();
    let s = s.strip_prefix('\'').ok_or(FilterParseError::Unsupported)?;
    let close = s.find('\'').ok_or(FilterParseError::UnclosedQuote)?;
    Ok(&s[..close])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_str_parses_starts_with() {
        // v0 grammar: pattern MUST end with `%`. The leading body
        // becomes the StartsWith prefix.
        let f = filter_str("source LIKE 'helios:fs/%'").unwrap();
        match f {
            Filter::StartsWith { field, prefix } => {
                assert_eq!(field, "source");
                assert_eq!(prefix, "helios:fs/");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn filter_str_parses_eq_string() {
        let f = filter_str("source = 'notes.md'").unwrap();
        match f {
            Filter::Eq { field, value } => {
                assert_eq!(field, "source");
                assert_eq!(value, serde_json::Value::String("notes.md".into()));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn filter_str_rejects_unsupported_glob() {
        let r = filter_str("source LIKE 'helios:fs/%.md'");
        assert!(matches!(r, Err(FilterParseError::UnsupportedGlob)));
    }

    #[test]
    fn filter_str_rejects_like_without_trailing_percent() {
        // 'helios:fs/' has NO trailing % → UnsupportedGlob.
        let r = filter_str("source LIKE 'helios:fs/'");
        assert!(matches!(r, Err(FilterParseError::UnsupportedGlob)));
        // Same for a bare word.
        let r = filter_str("source LIKE 'helios'");
        assert!(matches!(r, Err(FilterParseError::UnsupportedGlob)));
    }

    #[test]
    fn filter_str_rejects_unclosed_quote() {
        let r = filter_str("source = 'oops");
        assert!(matches!(r, Err(FilterParseError::UnclosedQuote)));
    }

    #[test]
    fn filter_str_rejects_garbage() {
        assert!(filter_str("").is_err());
        assert!(filter_str("DELETE FROM chunks").is_err());
        assert!(filter_str("source").is_err());
    }
}
