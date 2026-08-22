//! `Aggregate` — `FT.AGGREGATE` deterministic counting/grouping operator
//! (W5 task 3, Moon-only).
//!
//! ## Why
//!
//! Per `docs/benchmarks/v0.7-longmemeval-jscore-validation.md`, the single
//! largest LongMemEval generation-side miss bucket is counting/aggregation
//! questions ("how many times did X happen?") where retrieval was PERFECT
//! but the LLM miscounted the retrieved chunks. This operator lets a recipe
//! or MCP tool that has already classified a query as a counting question
//! issue Moon's native `FT.AGGREGATE` (`GROUPBY` / `REDUCE COUNT` /
//! `COUNT_DISTINCT` / `SUM` / `AVG` / `MIN` / `MAX`, optionally `SORTBY`) and
//! hand the reader a COMPUTED number instead of a pile of chunks to count.
//!
//! Classifying a user question as "this needs counting" is explicitly OUT
//! OF SCOPE here — see the module docs on [`Aggregate`] for the seam a
//! caller wires into.
//!
//! ## NOT a [`super::Retriever`]
//!
//! `Aggregate` does not participate in the `RawHit` / `Hit` fan-out — a
//! group-by result isn't a ranked list of memories, it's a small structured
//! table. Callers invoke [`Aggregate::execute`] directly against a
//! [`super::QueryContext`] once they've built or reused one (e.g. via
//! `RetrievalBuilder`'s Moon-wired storage), the same way `fusion.rs`'s
//! `fuse_via_moon_native` is a free function rather than a `Retriever` impl.
//!
//! ## Moon-only
//!
//! `FT.AGGREGATE` is issued through the typed moondb SDK
//! (`ctx.moon_storage`), the exact same access pattern
//! `fusion.rs::fuse_via_moon_native` uses for the native RRF hybrid path.
//! A `QueryContext` without a typed Moon handle wired (`ctx.moon_storage`
//! is `None` — e.g. a test double, or a future non-Moon backend) returns
//! `LunarisError::Storage(StorageError::NotSupported(_))` — never a silent
//! wrong (or worse, silently degraded/inexact) count. There is currently no
//! `StorageCapabilities` flag dedicated to aggregation (unlike
//! `native_rrf`); gating on `ctx.moon_storage.is_some()` is the best
//! available signal without editing `lunaris-core` (out of this crate's
//! ownership for this wave) — a future `StorageCapabilities::aggregate_native`
//! field would let this gate mirror `fuse_via_moon_native`'s
//! double-check exactly.
//!
//! ## v1 filter grammar is NOT the HYBRID FILTER tree
//!
//! `fusion.rs::filter_to_moon_hybrid_filter` builds a boolean `And`/`Or` tree
//! for the `HYBRID ... FILTER` clause. Moon's `FT.AGGREGATE` query-prefix
//! filter is a DIFFERENT, single-leaf grammar in v1 (see
//! `vendor/moon/src/command/vector_search/ft_text_search.rs::pre_parse_field_filter`):
//! `@field:{value}` (TAG exact match) or `@field:[lo hi]` (NUMERIC range).
//! Composing `And`/`Or` here would require Moon's top-level `FILTER` stage,
//! which is parsed-but-NO-OP server-side in v1 (I11/AGG-02) — so
//! `filter_to_aggregate_query` hard-errors on unsupported filter shapes
//! rather than silently dropping a constraint. An operator whose entire
//! purpose is a trustworthy number must never quietly loosen its filter.
//!
//! ## `APPLY` is out of scope
//!
//! Moon rejects `APPLY` at FT.AGGREGATE parse time ("ERR APPLY stage not
//! supported in v1") — this operator never emits it.

use std::collections::HashMap;

use lunaris_core::storage::types::Filter;
use lunaris_core::{LunarisError, RetrieveError, StorageError};

use super::QueryContext;

/// Which server-side reduction to compute per group. Mirrors moondb's
/// `Reducer` 1:1 (see `vendor/moon/sdk/rust/src/types.rs`) so callers of
/// this crate never need to depend on the vendored SDK type directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AggregateReducer {
    /// `REDUCE COUNT 0` — row count per group.
    Count,
    /// `REDUCE COUNT_DISTINCT 1 @field` — distinct value count per group.
    CountDistinct(String),
    /// `REDUCE SUM 1 @field`.
    Sum(String),
    /// `REDUCE AVG 1 @field`.
    Avg(String),
    /// `REDUCE MIN 1 @field`.
    Min(String),
    /// `REDUCE MAX 1 @field`.
    Max(String),
}

impl AggregateReducer {
    /// The result-row key Moon assigns this reducer when no explicit `AS`
    /// alias is given. The moondb SDK's `aggregate()` never sends `AS` (see
    /// `vendor/moon/sdk/rust/src/text.rs::TextClient::aggregate`), so Moon's
    /// `auto_alias()` convention
    /// (`vendor/moon/src/command/vector_search/ft_aggregate.rs`) is the
    /// ONLY naming scheme that ever reaches the wire through this operator:
    /// `COUNT` → `"count"`, `SUM`/`AVG`/`MIN`/`MAX`/`COUNT_DISTINCT` →
    /// `"<fn>_<field>"`.
    pub fn result_key(&self) -> String {
        match self {
            Self::Count => "count".to_string(),
            Self::CountDistinct(f) => format!("count_distinct_{f}"),
            Self::Sum(f) => format!("sum_{f}"),
            Self::Avg(f) => format!("avg_{f}"),
            Self::Min(f) => format!("min_{f}"),
            Self::Max(f) => format!("max_{f}"),
        }
    }

    fn into_moon(self) -> moon::types::Reducer {
        use moon::types::Reducer as R;
        match self {
            Self::Count => R::Count,
            Self::CountDistinct(f) => R::CountDistinct(f),
            Self::Sum(f) => R::Sum(f),
            Self::Avg(f) => R::Avg(f),
            Self::Min(f) => R::Min(f),
            Self::Max(f) => R::Max(f),
        }
    }
}

/// One `GROUPBY` group's reduced result row.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct AggregateGroup {
    /// The value of the `group_by` field for this group (e.g. `"open"`,
    /// `"helios:fs/notes.md"`).
    pub group_value: String,
    /// Every `REDUCE` column, keyed by [`AggregateReducer::result_key`].
    /// Values are the raw ASCII strings Moon returns — `COUNT` /
    /// `COUNT_DISTINCT` are integer-valued, `SUM`/`AVG`/`MIN`/`MAX` are
    /// float-valued. Use [`AggregateGroup::count_as_u64`] /
    /// [`AggregateGroup::value_as_f64`] to parse a specific reducer's column.
    pub values: HashMap<String, String>,
}

impl AggregateGroup {
    /// Parse the named reducer's result as `u64` — the natural type for
    /// `Count` / `CountDistinct`.
    pub fn count_as_u64(&self, reducer: &AggregateReducer) -> Option<u64> {
        self.values.get(&reducer.result_key())?.parse().ok()
    }

    /// Parse the named reducer's result as `f64` — the natural type for
    /// `Sum` / `Avg` / `Min` / `Max`.
    pub fn value_as_f64(&self, reducer: &AggregateReducer) -> Option<f64> {
        self.values.get(&reducer.result_key())?.parse().ok()
    }
}

/// `FT.AGGREGATE`-backed deterministic counting/grouping operator
/// (Moon-only). See module docs for why this is NOT a [`super::Retriever`].
///
/// ```ignore
/// use lunaris_retrieve::operators::aggregate::Aggregate;
///
/// // "how many episodes per source?"
/// let groups = Aggregate::count("chunks", "source").execute(&ctx).await?;
/// for g in &groups {
///     println!("{}: {}", g.group_value, g.count_as_u64(&lunaris_retrieve::operators::aggregate::AggregateReducer::Count).unwrap_or(0));
/// }
/// ```
#[derive(Clone, Debug)]
#[must_use = "Aggregate does nothing until you call .execute(ctx)"]
pub struct Aggregate {
    index: String,
    filter: Option<Filter>,
    group_by: String,
    reducers: Vec<AggregateReducer>,
    sort_by: Option<(String, bool)>,
    limit: Option<usize>,
}

impl Aggregate {
    /// Build a plain `COUNT` aggregate: `GROUPBY 1 @<group_by> REDUCE COUNT
    /// 0`, sorted by count descending. The common "how many X per Y" recipe.
    pub fn count(index: impl Into<String>, group_by: impl Into<String>) -> Self {
        Self {
            index: index.into(),
            filter: None,
            group_by: group_by.into(),
            reducers: vec![AggregateReducer::Count],
            sort_by: Some(("count".to_string(), false)),
            limit: None,
        }
    }

    /// Build a custom aggregate with an explicit reducer list — for callers
    /// that need `SUM`/`AVG`/`MIN`/`MAX`/`COUNT_DISTINCT` alongside or
    /// instead of `COUNT`. No default `SORTBY` / `LIMIT` — set them via
    /// [`Self::sort_by`] / [`Self::limit`] if needed.
    pub fn new(
        index: impl Into<String>,
        group_by: impl Into<String>,
        reducers: Vec<AggregateReducer>,
    ) -> Self {
        Self {
            index: index.into(),
            filter: None,
            group_by: group_by.into(),
            reducers,
            sort_by: None,
            limit: None,
        }
    }

    /// Narrow the aggregate to rows matching `filter`. v1 supports a single
    /// [`Filter::Eq`] (→ `@field:{value}` TAG clause) or
    /// [`Filter::ValidTimeRange`] (→ `@valid_time:[min max]` NUMERIC clause)
    /// leaf — see module docs for why `And`/`Or`/`StartsWith` composition is
    /// rejected rather than silently dropped.
    pub fn filter(mut self, filter: Filter) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Override the default `SORTBY` (field, ascending). `Aggregate::count`
    /// defaults to `("count", false)` (descending — biggest group first).
    /// `field` must be either the `group_by` field or a
    /// [`AggregateReducer::result_key`].
    pub fn sort_by(mut self, field: impl Into<String>, ascending: bool) -> Self {
        self.sort_by = Some((field.into(), ascending));
        self
    }

    /// Cap the number of returned groups.
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Run the aggregate against `ctx`. Moon-only — see module docs for the
    /// capability-gating rationale.
    pub async fn execute(&self, ctx: &QueryContext) -> Result<Vec<AggregateGroup>, LunarisError> {
        let moon =
            ctx.moon_storage.as_ref().ok_or(LunarisError::Storage(StorageError::NotSupported(
                "Aggregate operator requires a Moon backend (FT.AGGREGATE); \
             ctx.moon_storage is not wired for this QueryContext",
            )))?;

        let query = filter_to_aggregate_query(self.filter.as_ref())?;
        let moon_reducers: Vec<moon::types::Reducer> =
            self.reducers.iter().cloned().map(AggregateReducer::into_moon).collect();

        // RFC 0001 Wave 1C parity: the write path routes to the per-scope FT
        // index (`lunaris_{scope}_{kind}_idx`) — same per-scope naming
        // `fusion.rs::fuse_via_moon_native` builds for the hybrid path.
        let per_scope_index = format!("lunaris_{}_{}_idx", ctx.scope.as_str(), self.index);

        let typed = moon.client().typed();
        let mut text = typed.text();
        let rows = text
            .aggregate(
                &per_scope_index,
                &query,
                &self.group_by,
                &moon_reducers,
                self.sort_by.as_ref().map(|(f, asc)| (f.as_str(), *asc)),
                self.limit,
            )
            .await
            .map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("moon FT.AGGREGATE: {e}")))
            })?;

        // Moon's aggregate row carries the GROUPBY field under its
        // `@`-prefixed name (the SDK's `aggregate()` prefixes `group_by`
        // itself — see `text.rs`), the reducer columns under their
        // auto-generated aliases (no `@` prefix).
        let group_field_key = format!("@{}", self.group_by);
        Ok(rows
            .into_iter()
            .map(|row| {
                let mut values = row.fields;
                let group_value = values.remove(&group_field_key).unwrap_or_default();
                AggregateGroup { group_value, values }
            })
            .collect())
    }
}

/// Translate a Lunaris [`Filter`] into a Moon `FT.AGGREGATE` query-prefix
/// string. `None` ⇒ match-all (`"*"`). See module docs for why unsupported
/// shapes are a hard error, not a best-effort degrade.
fn filter_to_aggregate_query(filter: Option<&Filter>) -> Result<String, LunarisError> {
    match filter {
        None => Ok("*".to_string()),
        Some(Filter::Eq { field, value }) => Ok(format!("@{field}:{{{}}}", json_bare(value))),
        Some(Filter::ValidTimeRange { after, before }) => {
            // Half-open `[after, before)` — `hi - 1` on the upper bound,
            // matching every other render of this filter (F21).
            let min = after.map_or(0.0_f64, |h| h.wall_ms as f64);
            let max = before.map_or(u64::MAX as f64, |h| h.wall_ms.saturating_sub(1) as f64);
            Ok(format!("@valid_time:[{min} {max}]"))
        }
        Some(other) => Err(LunarisError::Retrieve(RetrieveError::OperatorFailed(format!(
            "Aggregate operator v1 supports only a single Eq/ValidTimeRange filter leaf \
             (Moon's FT.AGGREGATE query-prefix filter accepts exactly one @field:{{...}} or \
             @field:[lo hi] clause in v1 — top-level FILTER-stage boolean composition is \
             parsed-but-NO-OP server-side per I11/AGG-02); got {other:?}"
        )))),
    }
}

fn json_bare(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => format!("{v}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AggregateReducer::result_key ──

    #[test]
    fn result_key_matches_moon_auto_alias_convention() {
        assert_eq!(AggregateReducer::Count.result_key(), "count");
        assert_eq!(
            AggregateReducer::CountDistinct("user".into()).result_key(),
            "count_distinct_user"
        );
        assert_eq!(AggregateReducer::Sum("price".into()).result_key(), "sum_price");
        assert_eq!(AggregateReducer::Avg("price".into()).result_key(), "avg_price");
        assert_eq!(AggregateReducer::Min("price".into()).result_key(), "min_price");
        assert_eq!(AggregateReducer::Max("price".into()).result_key(), "max_price");
    }

    // ── Aggregate::count builder defaults ──

    #[test]
    fn count_defaults_to_single_count_reducer_sorted_desc() {
        let agg = Aggregate::count("chunks", "source");
        assert_eq!(agg.index, "chunks");
        assert_eq!(agg.group_by, "source");
        assert_eq!(agg.reducers, vec![AggregateReducer::Count]);
        assert_eq!(agg.sort_by, Some(("count".to_string(), false)));
        assert_eq!(agg.limit, None);
        assert!(agg.filter.is_none());
    }

    #[test]
    fn new_has_no_default_sort_or_limit() {
        let agg = Aggregate::new("chunks", "source", vec![AggregateReducer::Sum("bytes".into())]);
        assert_eq!(agg.sort_by, None);
        assert_eq!(agg.limit, None);
    }

    #[test]
    fn builder_methods_chain() {
        let agg = Aggregate::count("chunks", "source")
            .filter(Filter::Eq { field: "kind".into(), value: serde_json::json!("note") })
            .sort_by("source", true)
            .limit(10);
        assert!(agg.filter.is_some());
        assert_eq!(agg.sort_by, Some(("source".to_string(), true)));
        assert_eq!(agg.limit, Some(10));
    }

    // ── filter_to_aggregate_query ──

    #[test]
    fn filter_none_is_match_all() {
        assert_eq!(filter_to_aggregate_query(None).unwrap(), "*");
    }

    #[test]
    fn filter_eq_renders_tag_clause() {
        let f = Filter::Eq { field: "source".into(), value: serde_json::json!("notes.md") };
        assert_eq!(filter_to_aggregate_query(Some(&f)).unwrap(), "@source:{notes.md}");
    }

    #[test]
    fn filter_valid_time_range_renders_numeric_clause() {
        use lunaris_core::hlc::Hlc;
        let after = Hlc { wall_ms: 1_700_000_000_000, counter: 0, node_id: 0 };
        let before = Hlc { wall_ms: 1_760_000_000_000, counter: 0, node_id: 0 };
        let f = Filter::ValidTimeRange { after: Some(after), before: Some(before) };
        // Upper bound is `hi - 1`: the filter is documented half-open
        // `[after, before)` while Moon's numeric range is inclusive.
        // Re-baselined by F21 along with the other four render sites.
        assert_eq!(
            filter_to_aggregate_query(Some(&f)).unwrap(),
            "@valid_time:[1700000000000 1759999999999]"
        );
    }

    #[test]
    fn filter_valid_time_open_sides_use_finite_sentinels() {
        let f = Filter::ValidTimeRange { after: None, before: None };
        let s = filter_to_aggregate_query(Some(&f)).unwrap();
        assert_eq!(s, format!("@valid_time:[0 {}]", u64::MAX as f64));
    }

    #[test]
    fn filter_and_or_startswith_are_hard_errors_not_silent_drops() {
        // A counting operator that silently loosened its filter would return
        // a wrong-but-confident number — worse than an error.
        let and_filter = Filter::And(vec![
            Filter::Eq { field: "a".into(), value: serde_json::json!("1") },
            Filter::Eq { field: "b".into(), value: serde_json::json!("2") },
        ]);
        assert!(filter_to_aggregate_query(Some(&and_filter)).is_err());

        let or_filter = Filter::Or(vec![
            Filter::Eq { field: "a".into(), value: serde_json::json!("1") },
            Filter::Eq { field: "b".into(), value: serde_json::json!("2") },
        ]);
        assert!(filter_to_aggregate_query(Some(&or_filter)).is_err());

        let starts_with = Filter::StartsWith { field: "source".into(), prefix: "helios".into() };
        assert!(filter_to_aggregate_query(Some(&starts_with)).is_err());
    }
}
