//! Plan 16-00 Task 1 — RED: compile-time proof that `ActRConsolidator` is a
//! real type implementing [`lunaris_consolidate::Consolidator`] + `Send +
//! Sync + 'static`.
//!
//! This test fails at compile time when Plan 16-00 is un-authored — the
//! `ActRConsolidator` symbol does not exist. Task 2 (GREEN) authors the
//! concrete struct + `impl Consolidator` in `crates/lunaris-consolidate/src/
//! act_r.rs` and re-exports it from the crate root, flipping this test
//! green without changing a line of this file.
//!
//! The bound set `Consolidator + Send + Sync + 'static` is the exact
//! contract the worker loop requires when storing an `Arc<dyn Consolidator>`
//! in [`ConsolidatorPipelineHandle`]. Locking it here means the authored
//! type can be swapped into the handle without a second audit of its
//! auto-traits.

#[test]
fn act_r_consolidator_exists_and_impls_consolidator() {
    fn _assert_impl<T: lunaris_consolidate::Consolidator + Send + Sync + 'static>() {}
    _assert_impl::<lunaris_consolidate::ActRConsolidator>();
}
