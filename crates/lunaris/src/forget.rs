//! Plan 04-05 — `Lunaris::forget(target) -> ForgetReceipt`.
//!
//! ## B-7 forward-compat stub (Task 1 commit)
//!
//! This file currently ships only the public types `ForgetTarget`,
//! `ScopeSpec`, `IndexKind`, `ForgetReceipt`, `ForgetConfirmation` so that
//! `crates/lunaris/src/audit.rs` (Task 1) can `use crate::forget::ForgetReceipt`
//! and `cargo check -p lunaris` exits 0 after Task 1 alone. Plan 04-05 Task 2
//! REPLACES the stub with the full body:
//!
//! - `ForgetRequest` builder + `.hard()` / `.dry_run()` methods
//! - `Lunaris::forget(target)` impl
//! - `Lunaris::confirm_hard_forget(receipt)` impl
//! - `scan_matches` + `match_scope` + `match_before` (W-1 typed Hlc compare)
//! - `build_soft_delete_op` (B-4 `clock.tick()` + B-5 `BiTemporal::invalidate_sys`)
//!
//! The public types declared here lock the wire shape; Task 2 only adds the
//! impl block + helpers without changing the existing struct/enum surface.

use lunaris_core::Hlc;
use lunaris_core::storage::types::Lsn;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Single-entry-point target for `Lunaris::forget` per D-18. Three variants
/// closing OPS-01 / OPS-02 / OPS-03.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgetTarget {
    /// OPS-01: single-target purge across KV + vector + graph indices.
    Id(Ulid),
    /// OPS-02: scope purge — soft-delete by default; `.hard()` requires
    /// confirmation token (D-21 safety rail).
    Scope(ScopeSpec),
    /// OPS-03: temporal-bound purge using AS_OF semantics (D-19).
    Before(Hlc),
}

/// Match language for `ForgetTarget::Scope` per D-20. v0 supports prefix-match
/// on `source` (the helios:fs/ session-pruning case), exact metadata kv match,
/// and exact episode-id match. Richer predicate languages are v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeSpec {
    /// Prefix match on the JSON `source` field (e.g., `"helios:fs/session-42/"`).
    BySource(String),
    /// Exact match on a single `metadata.<key> == <value>` pair.
    ByMetadata(String, String),
    /// Exact match on the JSON `id` field.
    ByEpisode(Ulid),
}

/// Tag for which storage indices a forget call touched. Mirrors the
/// `lunaris_consolidate::types::IndexKind` shape so Plan 04-05 can wire them
/// 1:1 when the audit emit surface is unified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKind {
    Kv,
    Vector,
    Graph,
}

/// Receipt returned by every `Lunaris::forget` call. Carries enough
/// information for the caller to reconstruct what was attempted (preview)
/// vs what was committed (rows_written / rows_deleted).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetReceipt {
    pub target: ForgetTarget,
    pub indices_affected: Vec<IndexKind>,
    /// Soft-delete MVCC writes (zero for hard / dry-run).
    pub rows_written: u64,
    /// Irreversible deletes (hard-only; zero for soft / dry-run).
    pub rows_deleted: u64,
    /// `__lunaris_audit__` publish offset.
    pub audit_lsn: Lsn,
    /// `true` iff this was a `.dry_run()` preview that did NOT call
    /// `atomic_write`.
    pub preview: bool,
}

/// Opaque confirmation token returned by `Lunaris::confirm_hard_forget(dry_run)`
/// per D-21 hard-delete safety rail. Cannot be constructed by the caller —
/// only returned from the two-step protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetConfirmation {
    pub(crate) for_audit_lsn: Lsn,
}
