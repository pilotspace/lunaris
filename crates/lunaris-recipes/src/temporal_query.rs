//! Phase 9 Plan 09-04 PRIM-03 — `TemporalQuery<S>` typestate primitive.
//!
//! Time-travel combinator where `S` is a compile-only phantom marker —
//! `Messages` / `Documents` / `Facts`. Method availability is bound by
//! sealed traits `SupportsAsOf` + `SupportsBetween`, so invalid
//! combinations fail at `cargo check` rather than at runtime. Mirrors
//! the `tower::Service` typestate shape per ROADMAP Phase 9 risk register
//! row #4.
//!
//! ## Phase 9 scope vs Phase 9.1 scope
//!
//! Phase 9 (this plan) ships the typestate infrastructure + `.as_of`
//! wiring to `RetrievalBuilder::as_of`. `.before`, `.after`, `.between`
//! accumulate into `TemporalBounds` but are NOT surfaced to the backend
//! until Phase 9.1 Plan 09.1-02 adds `Filter::ValidTimeRange` +
//! `RetrievalBuilder::valid_time_range` predicate. Phase 9.1 is a
//! cross-crate change (`lunaris-core` + 2 storage crates + retrieve) so
//! it is NOT shipped here per orchestrator split decision 2026-04-22.
//!
//! The typestate shape in this plan is final — Phase 9.1 only adds the
//! backend wiring inside `.execute()` and the corresponding tests.
//!
//! ## Why typestate, not runtime enum
//!
//! A naive `.between(a, b)` on a source that only makes sense with
//! `as_of` (e.g. a future "StreamOfEvents" source) must fail at compile
//! time. Phantom markers + sealed traits achieve this with zero runtime
//! cost and no unsafe.
//!
//! ## Backend dispatch
//!
//! Moon `TEMPORAL.SNAPSHOT_AT` and Postgres bi-temporal `WHERE valid_start
//! <= ts AND (valid_end IS NULL OR valid_end > ts)` are BOTH handled by
//! [`lunaris_retrieve::RetrievalBuilder::execute`] per blueprint §5.3.
//! This primitive does NOT branch on backend.
//!
//! ## Usage
//!
//! ```ignore
//! use lunaris_recipes::{TemporalQuery, Messages};
//! let hits = TemporalQuery::<Messages>::new(mem)
//!     .as_of(some_ts)
//!     .execute("search text")
//!     .await?;
//! ```
//!
//! ## ≤ 30 LOC public-surface contract (PRIM-03)
//!
//! Public symbols: `new`, `as_of`, `before`, `after`, `between`, `execute`.
//! Six `pub fn` / `pub async fn`; LOC guard caps at 6 (tight — but this
//! primitive owns the bound so no future method additions).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::marker::PhantomData;
use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::LunarisError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::Filter;
use lunaris_retrieve::{Hit, Query};

/// Sealed-trait module. External crates cannot impl `SupportsAsOf` or
/// `SupportsBetween` for their own types — Lunaris owns the source set.
mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Messages {}
    impl Sealed for super::Documents {}
    impl Sealed for super::Facts {}
}

/// Source marker: conversational message stream. Zero-sized phantom.
pub struct Messages;

/// Source marker: documentary corpus. Zero-sized phantom.
pub struct Documents;

/// Source marker: consolidated fact store. Zero-sized phantom.
pub struct Facts;

/// Capability: supports point-in-time (`as_of`, `before`, `after`) queries.
/// Sealed — cannot be implemented outside this crate.
pub trait SupportsAsOf: sealed::Sealed {}
impl SupportsAsOf for Messages {}
impl SupportsAsOf for Documents {}
impl SupportsAsOf for Facts {}

/// Capability: supports range (`between(a, b)`) queries. Sealed.
pub trait SupportsBetween: sealed::Sealed {}
impl SupportsBetween for Messages {}
impl SupportsBetween for Documents {}
impl SupportsBetween for Facts {}

/// Accumulated temporal bounds — all Option so any combination of
/// operators produces a well-formed query at `.execute()`. In Phase 9
/// only `as_of` flushes to the backend; `before`/`after` are recorded
/// and are backend-wired in Phase 9.1 Plan 09.1-02.
#[derive(Default, Clone, Debug)]
struct TemporalBounds {
    as_of: Option<Hlc>,
    before: Option<Hlc>,
    after: Option<Hlc>,
}

/// Crate-private bounds check factored out of `between` (Blocker #3 fix).
/// The production assertion IS the test — see
/// `temporal_query_between_requires_both_bounds_valid` below, which
/// invokes this free fn with swapped args. This prevents the prior
/// "self-fulfilling test" anti-pattern where production asserts were
/// never exercised.
///
/// Compares `(wall_ms, counter)` tuples because `Hlc` has no `.millis()`
/// method — the struct exposes `pub wall_ms: u64` + `pub counter: u32`
/// directly (see `crates/lunaris-core/src/hlc.rs:20-25`) and derives
/// `PartialOrd + Ord` lexicographically.
fn check_between_bounds(after: Hlc, before: Hlc) {
    assert!(
        (after.wall_ms, after.counter) <= (before.wall_ms, before.counter),
        "TemporalQuery::between — after ({}.{}) must be ≤ before ({}.{})",
        after.wall_ms,
        after.counter,
        before.wall_ms,
        before.counter
    );
}

/// Typestate time-travel combinator. `S` is a compile-only phantom.
pub struct TemporalQuery<S> {
    lunaris: Arc<Lunaris>,
    bounds: TemporalBounds,
    _source: PhantomData<S>,
}

impl<S> TemporalQuery<S>
where
    S: SupportsAsOf,
{
    /// Construct a TemporalQuery over source `S`. Source is a phantom
    /// marker — no runtime value required.
    pub fn new(lunaris: Arc<Lunaris>) -> Self {
        Self { lunaris, bounds: TemporalBounds::default(), _source: PhantomData }
    }

    /// Return the snapshot at `ts` for source `S`. Subsequent operators
    /// compose on the same builder. Fully wired in Phase 9.
    pub fn as_of(mut self, ts: Hlc) -> Self {
        self.bounds.as_of = Some(ts);
        self
    }

    /// Constrain to items whose valid_time < `ts`. Recorded in
    /// `TemporalBounds.before`; backend wiring lands in Phase 9.1 Plan
    /// 09.1-02 (`Filter::ValidTimeRange`). Calling `.before` in Phase 9
    /// does not affect `.execute()` — this is DOCUMENTED, not silent
    /// drop.
    pub fn before(mut self, ts: Hlc) -> Self {
        self.bounds.before = Some(ts);
        self
    }

    /// Constrain to items whose valid_time > `ts`. Phase 9.1 wires
    /// backend support — see `before` docstring.
    pub fn after(mut self, ts: Hlc) -> Self {
        self.bounds.after = Some(ts);
        self
    }

    /// Execute the accumulated temporal query against source `S`.
    /// Backend dispatch is handled by `RetrievalBuilder::execute`.
    ///
    /// Phase 9 flushes `as_of` only. Phase 9.1 extends this method to
    /// emit `Filter::ValidTimeRange { after, before }` when either bound
    /// is set. Emitting partial `Filter::ValidTimeRange` in Phase 9
    /// without the Filter variant in `lunaris-core` would not compile;
    /// the PR deliberately holds for Phase 9.1.
    pub async fn execute(self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        let mut builder = self.lunaris.recall();
        if let Some(ts) = self.bounds.as_of {
            builder = builder.as_of(ts);
        }
        // Phase 9.1 Plan 09.1-02 — emit Filter::ValidTimeRange when either
        // bound is set. The if-guard keeps the filter out of the path for
        // pure `.as_of` queries (zero regression on Phase 9 PRIM-03
        // structural behaviour). Renderers: Moon `@valid_time:[lo hi]`
        // (vector.rs / keyword.rs) + Postgres `valid_from >= 'rfc' AND
        // valid_from < 'rfc'` (vector.rs / keyword.rs).
        if self.bounds.before.is_some() || self.bounds.after.is_some() {
            builder = builder.filter(Filter::ValidTimeRange {
                after: self.bounds.after,
                before: self.bounds.before,
            });
        }
        builder.execute(Query::text(query)).await
    }
}

/// `.between(a, b)` lives on a separate impl so the `SupportsBetween`
/// bound is visible at the trait level. Typestate win: any future
/// source that does NOT impl SupportsBetween cannot call `between`.
impl<S> TemporalQuery<S>
where
    S: SupportsAsOf + SupportsBetween,
{
    /// Constrain to items whose valid_time falls within `[after, before]`.
    /// Panics if `after > before` — check is factored into
    /// [`check_between_bounds`] (Blocker #3 fix) so the test exercises
    /// the production assertion directly.
    pub fn between(mut self, after: Hlc, before: Hlc) -> Self {
        check_between_bounds(after, before);
        self.bounds.after = Some(after);
        self.bounds.before = Some(before);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporal_query_public_surface_under_30_loc() {
        let src = include_str!("./temporal_query.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "PRIM-03 ≤30-LOC contract: TemporalQuery has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 4,
            "PRIM-03 contract: TemporalQuery needs at least 4 public methods; got {pub_fns}"
        );
    }

    #[test]
    fn temporal_query_bounds_accumulate() {
        let b = TemporalBounds {
            as_of: Some(Hlc::from_parts(1_000, 0, 0)),
            before: Some(Hlc::from_parts(2_000, 0, 0)),
            after: None,
        };
        assert!(b.as_of.is_some());
        assert!(b.before.is_some());
        assert!(b.after.is_none());
    }

    /// Blocker #3 fix — production assertion IS the test.
    ///
    /// Calls `check_between_bounds(later, earlier)` with swapped args.
    /// The `#[should_panic(expected = ...)]` matches the panic message
    /// produced by the crate-private free fn invoked by
    /// `TemporalQuery::between`. No synthetic `panic!` in the test
    /// body; no trivially-true inequality short-circuit.
    #[test]
    #[should_panic(expected = "TemporalQuery::between")]
    fn temporal_query_between_requires_both_bounds_valid() {
        let earlier = Hlc::from_parts(1_000, 0, 0);
        let later = Hlc::from_parts(2_000, 0, 0);
        // after > before — must panic.
        check_between_bounds(later, earlier);
    }

    /// Production invariant sanity: valid ranges must NOT panic.
    #[test]
    fn temporal_query_between_accepts_valid_range() {
        let earlier = Hlc::from_parts(1_000, 0, 0);
        let later = Hlc::from_parts(2_000, 0, 0);
        // after < before — must not panic.
        check_between_bounds(earlier, later);
    }

    /// Production invariant sanity: equal endpoints are allowed.
    #[test]
    fn temporal_query_between_accepts_equal_endpoints() {
        let ts = Hlc::from_parts(1_500, 0, 0);
        check_between_bounds(ts, ts);
    }

    #[test]
    fn temporal_query_sealed_trait_is_crate_private() {
        // Sanity: the sealed::Sealed trait lives in a private module,
        // so the compiler forbids external impls. This test documents
        // the intent — the actual enforcement is compile-time.
        fn assert_sealed<T: sealed::Sealed>() {}
        assert_sealed::<Messages>();
        assert_sealed::<Documents>();
        assert_sealed::<Facts>();
    }

    #[test]
    fn temporal_query_all_sources_support_as_of_and_between() {
        fn assert_as_of<T: SupportsAsOf>() {}
        fn assert_between<T: SupportsBetween>() {}
        assert_as_of::<Messages>();
        assert_as_of::<Documents>();
        assert_as_of::<Facts>();
        assert_between::<Messages>();
        assert_between::<Documents>();
        assert_between::<Facts>();
    }
}
