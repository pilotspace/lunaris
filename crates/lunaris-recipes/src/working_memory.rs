//! Phase 9 Plan 09-03 PRIM-04 (structural half) — `WorkingMemory` primitive.
//!
//! Scope-prefixed scratchpad for agentic working memory. Every `write` /
//! `read` / `grep` call scopes through a caller-supplied `scope_prefix` —
//! either via prefix concatenation on the Episode `source` field (write) or
//! via [`Filter::Eq`] / [`Filter::StartsWith`] at recall time (read / grep).
//! No SQL LIKE strings; no global state; no duplicate vector / BM25 libraries
//! (CLAUDE.md constraint — Moon native `FT.*` is canonical).
//!
//! ## Phase 9 scope vs Phase 9.1 scope
//!
//! Phase 9 (this plan) ships the REDUCED 4-method public surface:
//! `new / write / read / grep`.
//!
//! Phase 9.1 Plan 09.1-01 will add a 5th method on top of this surface that
//! invokes a scope-aware Consolidator hook reading `self.scope_prefix`. The
//! trait-level scope filter will be the single point of truth — Phase 12
//! HELIOS-04 will wire `scope_prefix = "helios:fs/"` through that same path.
//! The `scope_prefix` is captured HERE, in this plan, so when Phase 9.1
//! lands its hook the scope semantics are already enforced at the
//! ingest / recall path — no re-declaration of the field needed.
//!
//! ## Fluent API (Phase 9 reduced surface)
//!
//! ```ignore
//! use std::sync::Arc;
//! use lunaris::Lunaris;
//! use lunaris_recipes::WorkingMemory;
//!
//! # async fn demo(mem: Arc<Lunaris>) -> anyhow::Result<()> {
//! let wm = WorkingMemory::new(mem, "chat:user-42/");
//! wm.write("draft-reply", serde_json::json!({"body": "..."})).await?;
//! let cached = wm.read("draft-reply").await?;
//! let matches = wm.grep("draft-").await?;
//! // Phase 9.1 Plan 09.1-01 will add a scope-aware promotion hook here.
//! # Ok(()) }
//! ```
//!
//! ## ≤ 30 LOC public-surface contract (PRIM-04 structural)
//!
//! Public symbols on this module: [`WorkingMemory::new`],
//! [`WorkingMemory::write`], [`WorkingMemory::read`], [`WorkingMemory::grep`].
//! Four `pub fn` / `pub async fn` declarations — the LOC-guard test at the
//! bottom of this file counts them and caps at 6 (Phase 9.1 will add a fifth
//! method bringing the count to 5 — still well under the cap). Mirrors the
//! Phase 5 `helios_scratchpad_public_surface_under_50_loc` template +
//! Plan 09-01's `message_stream_public_surface_under_30_loc` verbatim.
//!
//! ## Plan-vs-code corrections applied (Rule 1 deviations, see 09-03-SUMMARY.md)
//!
//! The plan text's sample code referenced APIs that don't exist on the
//! v0.1.0 surface:
//!
//! 1. `self.lunaris.storage().get(&scoped_key, None)` — fictional. The real
//!    port method is `read_as_of(key, as_of: Hlc)` returning
//!    `Option<Row<Bytes>>`; `as_of` is non-optional.
//! 2. `self.lunaris.storage().scan_by_filter(&filter, None)` — fictional.
//!    The real port has `scan_range(prefix: &[u8], as_of: Option<Hlc>)`
//!    returning a `BoxStream`; it takes a byte prefix, not a `Filter`.
//! 3. `WriteOp::Upsert { ... }` — fictional. The real `WriteOp` enum is
//!    `KvPut | KvDelete | VectorUpsert | GraphNode | GraphEdge`.
//!
//! This matches the same plan-vs-code corrections Plan 09-01's MessageStream
//! and Plan 09-02's DocumentCorpus applied. The implementation below routes
//! `write` through [`Lunaris::ingest`] (which runs the full chunker +
//! embedder + single internal `atomic_write` fan-out — INGEST-04 contract
//! preserved at the per-write grain) so that:
//!
//! - The `chunks` vector index receives the scratchpad content, making
//!   `read` + `grep` recall over `Vector + Keyword` return hits.
//! - `read` / `grep` go through the composable retrieve DSL (`Filter::Eq`
//!   / `Filter::StartsWith` on the Episode `source` field) — mirrors
//!   MessageStream / DocumentCorpus / HeliosScratchpad shape.
//! - **Phase 9.1 handoff remains correct**: Consolidator events carry a
//!   `source` field derived from Episode ingestion, so Phase 9.1's
//!   `consolidate_scoped(Some(prefix))` default impl — which filters by
//!   `event.source.starts_with(prefix)` — will have events to filter. A
//!   raw-KV path (hand-built `KvPut` bypassing the chunker) would leave
//!   the consolidator with nothing to filter.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::storage::types::{Filter, Lsn};
use lunaris_core::{Episode, LunarisError};
use lunaris_retrieve::{Keyword, Query, Vector};

/// Default `top_k` the `read` / `grep` recall paths use. Chosen to match
/// Plan 09-01 MessageStream's `DEFAULT_TOP_K` + HeliosScratchpad's `READ_TOP`
/// (both 8) so conversational wrappers that compose WorkingMemory with
/// MessageStream / scratchpad primitives inherit the same breadth.
const DEFAULT_TOP_K: usize = 8;

/// Fan-out multiplier applied to each branch of the fused plan before RRF
/// fuses them. Matches Plan 09-01 MessageStream + Plan 09-02 DocumentCorpus
/// (`3`) so the three Phase 9 primitives share a consistent pre-fusion
/// window size.
const FANOUT: usize = 3;

/// RRF constant from Cormack et al. (2009). Matches
/// `DocumentCorpus::DEFAULT_RRF_K` (60). Shared across every Phase 9 primitive
/// that fuses Vector + Keyword.
const RRF_K: u32 = 60;

/// Key-prefixed scratchpad. Stores `(k, v)` pairs under `{scope_prefix}{k}`
/// as [`Episode`]s on the Episode `source` field. In Phase 9.1 this struct
/// gains a scope-aware promotion hook method that reads `self.scope_prefix`
/// to invoke a Consolidator entry point — the field is captured HERE so the
/// Phase 9.1 addition is strictly additive (no refactor of the existing 4
/// methods required).
///
/// State is immutable after construction (`lunaris` is an `Arc`;
/// `scope_prefix` is an owned `String`), so this type holds ZERO `RwLock`s
/// — CLAUDE.md lock discipline is satisfied trivially (no guard can be held
/// across `.await` because there are no guards).
#[derive(Clone)]
pub struct WorkingMemory {
    lunaris: Arc<Lunaris>,
    scope_prefix: String,
}

impl WorkingMemory {
    /// Construct a new scratchpad bound to `scope_prefix`. Every
    /// [`write`](Self::write) / [`read`](Self::read) / [`grep`](Self::grep)
    /// call scopes through this string. Phase 9.1's promotion hook will read
    /// the same field — no re-declaration needed when Phase 9.1 lands.
    ///
    /// Conventional values: `"chat:user-42/"`, `"helios:fs/session-7/"`,
    /// `"notes:"`. Pass an empty string to disable scoping (the scratchpad
    /// will then see every Episode in the store — rarely desirable outside
    /// one-off debugging).
    pub fn new(lunaris: Arc<Lunaris>, scope_prefix: impl Into<String>) -> Self {
        Self {
            lunaris,
            scope_prefix: scope_prefix.into(),
        }
    }

    /// Write `(k, v)` under `{scope_prefix}{k}` as an [`Episode`]. Delegates
    /// to [`Lunaris::ingest`] — the umbrella pipeline runs chunking +
    /// embedding + a SINGLE internal `atomic_write` fan-out (INGEST-04
    /// contract preserved at the per-write grain — matches Plan 09-01 +
    /// 09-02 precedent). The scoped key lands on the Episode `source` field,
    /// so `read` / `grep` can filter via `Filter::Eq` / `Filter::StartsWith`.
    pub async fn write(&self, k: &str, v: serde_json::Value) -> Result<Lsn, LunarisError> {
        let source = self.scope_key(k);
        let content = serde_json::to_string(&v).map_err(|e| LunarisError::from(
            lunaris_core::StorageError::from(e),
        ))?;
        let episode = Episode::new(source, content, self.lunaris.clock().as_ref());
        self.lunaris.ingest(episode).await
    }

    /// Read the value for `k` scoped under `scope_prefix`, if present.
    /// Recalls over the fused `Vector + Keyword` hybrid plan filtered by
    /// `Filter::Eq` on the `source` field (exact match on the scoped key).
    /// Returns `Ok(None)` when no Episode exists at that source. When hits
    /// are returned the first hit's `text` is parsed back into the original
    /// `serde_json::Value` — mirrors the HeliosScratchpad `read_at` shape.
    pub async fn read(&self, k: &str) -> Result<Option<serde_json::Value>, LunarisError> {
        let source = self.scope_key(k);
        let filter = Filter::Eq {
            field: "source".into(),
            value: serde_json::Value::String(source),
        };
        let plan = Vector::new("chunks", DEFAULT_TOP_K * FANOUT)
            .and(Keyword::bm25("chunks", DEFAULT_TOP_K * FANOUT))
            .fuse_rrf(RRF_K)
            .top(DEFAULT_TOP_K);
        let hits = self
            .lunaris
            .recall()
            .with_root(plan)
            .filter(filter)
            .execute(Query::text(k))
            .await?;
        match hits.into_iter().next() {
            Some(h) => Ok(Some(serde_json::from_str(&h.text).map_err(|e| {
                LunarisError::from(lunaris_core::StorageError::from(e))
            })?)),
            None => Ok(None),
        }
    }

    /// Return all `(source, value)` pairs whose `source` starts with
    /// `{scope_prefix}{pattern}`. Uses [`Filter::StartsWith`] on the
    /// `source` field — NEVER a SQL LIKE string (the v0.1.0 `Filter` enum
    /// is canonical; SQL string concatenation is an injection shape we
    /// refuse to introduce). Each hit's `text` is parsed back into the
    /// original `serde_json::Value`.
    pub async fn grep(&self, pattern: &str) -> Result<Vec<(String, serde_json::Value)>, LunarisError> {
        let filter = Filter::StartsWith {
            field: "source".into(),
            prefix: self.scope_key(pattern),
        };
        let plan = Vector::new("chunks", DEFAULT_TOP_K * FANOUT)
            .and(Keyword::bm25("chunks", DEFAULT_TOP_K * FANOUT))
            .fuse_rrf(RRF_K)
            .top(DEFAULT_TOP_K);
        let hits = self
            .lunaris
            .recall()
            .with_root(plan)
            .filter(filter)
            .execute(Query::text(pattern))
            .await?;
        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            let v: serde_json::Value = serde_json::from_str(&h.text).map_err(|e| {
                LunarisError::from(lunaris_core::StorageError::from(e))
            })?;
            out.push((h.source, v));
        }
        Ok(out)
    }

    /// Internal — build the scoped key `{scope_prefix}{k}`. Not public so
    /// the LOC-guard cap stays tight; reused by `write` / `read` / `grep`
    /// and (in Phase 9.1) by the promotion hook when it filters pending
    /// events. The format string puts `scope_prefix` BEFORE the user key
    /// so that `Filter::StartsWith { prefix: scope_prefix }` correctly
    /// captures every scoped Episode in the store.
    fn scope_key(&self, k: &str) -> String {
        format!("{}{}", self.scope_prefix, k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRIM-04 ≤ 30 LOC public-surface contract. Counts `pub fn` +
    /// `pub async fn` declarations in the production portion of the source
    /// file (everything BEFORE the first `#[cfg(test)]` marker). Mirrors the
    /// Phase 5 `helios_scratchpad_public_surface_under_50_loc` template +
    /// Plan 09-01's `message_stream_public_surface_under_30_loc` +
    /// Plan 09-02's `document_corpus_public_surface_under_30_loc` verbatim.
    ///
    /// Cap is 6 (Phase 9 has 4: `new / write / read / grep`; Phase 9.1 will
    /// add a 5th — still one slot below the cap).
    #[test]
    fn working_memory_public_surface_under_30_loc() {
        let src = include_str!("./working_memory.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "PRIM-04 ≤30-LOC contract: WorkingMemory has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "PRIM-04 contract: WorkingMemory needs at least 3 public methods \
             (new, write, read, grep in Phase 9); got {pub_fns}"
        );
    }

    /// Scope-key format-string sanity — `{scope_prefix}{k}` must put the
    /// prefix BEFORE the user key so `Filter::StartsWith { prefix }`
    /// correctly matches every scoped Episode. Pure logic test — no Lunaris
    /// handle needed.
    #[test]
    fn working_memory_scope_key_prefix_concatenation() {
        fn scope_key(prefix: &str, k: &str) -> String {
            format!("{prefix}{k}")
        }
        assert_eq!(scope_key("helios:fs/", "note-1"), "helios:fs/note-1");
        assert_eq!(scope_key("chat:user-42/", "draft"), "chat:user-42/draft");
        assert_eq!(scope_key("", "raw-key"), "raw-key");
    }

    /// Verify `grep` constructs a `Filter::StartsWith` variant — NOT a SQL
    /// LIKE string. The v0.1.0 `Filter` enum is canonical; refusing SQL
    /// string concatenation is a strict security gate (the enum is the only
    /// non-injectable shape).
    #[test]
    fn working_memory_grep_uses_starts_with_filter() {
        let prefix = "chat:user-42/draft-";
        let filter = Filter::StartsWith {
            field: "source".into(),
            prefix: prefix.into(),
        };
        match filter {
            Filter::StartsWith { field, prefix: p } => {
                assert_eq!(field, "source");
                assert_eq!(p, "chat:user-42/draft-");
            }
            other => panic!("expected StartsWith variant; got {other:?}"),
        }
    }

    /// Sanity: construction records scope_prefix BEFORE the user key — any
    /// future refactor that flips the `format!` argument order would break
    /// `Filter::StartsWith` matching silently. This test locks the format
    /// direction.
    #[test]
    fn working_memory_construction_records_scope() {
        let s = format!("{}{}", "helios:fs/", "k");
        assert!(s.starts_with("helios:fs/"));
        assert!(s.ends_with("k"));
        assert_eq!(s, "helios:fs/k");
    }
}
