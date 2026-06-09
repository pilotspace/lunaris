//! Phase 9 Plan 09-03 PRIM-04 (structural half) — `WorkingMemory` primitive.
//!
//! Scope-prefixed scratchpad for agentic working memory. Every `write` /
//! `read` / `grep` call scopes through a caller-supplied `scope_prefix` —
//! either via prefix concatenation on the Episode `source` field (write) or
//! via [`Filter::Eq`] / [`Filter::StartsWith`] at recall time (read / grep).
//! No SQL LIKE strings; no global state; no duplicate vector / BM25 libraries
//! (CLAUDE.md constraint — Moon native `FT.*` is canonical).
//!
//! ## Relocation note (Phase 12 Option A)
//!
//! This type moved from `lunaris-recipes::working_memory` into
//! `lunaris::primitives::working_memory` so that Phase 12 `CodingSessionMemory`
//! (which lives in the `lunaris` crate) can compose over it without
//! introducing a `lunaris → lunaris-recipes` dependency cycle. The
//! `lunaris-recipes` crate re-exports this type verbatim — every Phase 9 /
//! 10 / 11 caller that imports `lunaris_recipes::WorkingMemory` keeps
//! compiling unchanged. Phase 13's proper primitives-crate extraction
//! subsumes this location.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use lunaris_consolidate::{
    CONSOLIDATE_CONSUMER_GROUP, CONSOLIDATE_TOPIC, ConsolidateEvent, ConsolidationReport,
};
use lunaris_core::keyspace::episode_key;
use lunaris_core::storage::types::{Filter, Lsn};
use lunaris_core::{Episode, HlcClock, LunarisError, Scope, StorageError, StoragePort};
use lunaris_retrieve::{Hit, Keyword, Query, Vector};
use ulid::Ulid;

use crate::Lunaris;

/// Phase 9.1 Plan 01 Task 3 — maximum events drained per
/// [`WorkingMemory::consolidate`] call. Bounds T-09-1-01-04 DoS surface:
/// heavy callers should invoke repeatedly rather than raising the cap.
const DRAIN_CAP: usize = 1024;

/// Phase 9.1 Plan 01 Task 3 — per-pull timeout on the drain stream. The
/// drain exits on the first timeout, stream end, or error; combined with
/// [`DRAIN_CAP`] this guarantees the drain terminates within
/// `DRAIN_CAP × PULL_TIMEOUT_MS` ms (worst case ≈ 51 s when the broker
/// keeps delivering events at exactly the timeout boundary).
const PULL_TIMEOUT_MS: u64 = 50;

/// Default `top_k` the `read` / `grep` recall paths use. Chosen to match
/// Plan 09-01 MessageStream's `DEFAULT_TOP_K` + CodingSessionMemory's `READ_TOP`
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
/// as [`Episode`]s on the Episode `source` field.
#[derive(Clone)]
pub struct WorkingMemory {
    lunaris: Arc<Lunaris>,
    scope: Scope,
    scope_prefix: String,
}

impl WorkingMemory {
    /// Construct a new scratchpad bound to `scope` (RFC 0001 partition key)
    /// and `scope_prefix` (source-key namespace). The two concepts are
    /// orthogonal: `scope` partitions the KV / FT keyspace, while
    /// `scope_prefix` namespaces the `source` field on each Episode so a
    /// single scope can host multiple WorkingMemory instances (e.g.,
    /// `"helios:fs/"` vs `"chat:user-42/"`).
    pub fn new(lunaris: Arc<Lunaris>, scope: Scope, scope_prefix: impl Into<String>) -> Self {
        Self { lunaris, scope, scope_prefix: scope_prefix.into() }
    }

    /// Write `(k, v)` under `{scope_prefix}{k}` as an [`Episode`].
    pub async fn write(&self, k: &str, v: serde_json::Value) -> Result<Lsn, LunarisError> {
        let source = self.scope_key(k);
        let content = serde_json::to_string(&v)
            .map_err(|e| LunarisError::from(lunaris_core::StorageError::from(e)))?;
        let episode =
            Episode::new(self.scope.clone(), source, content, self.lunaris.clock().as_ref());
        self.lunaris.ingest(episode).await
    }

    /// Read the value for `k` scoped under `scope_prefix`, if present.
    ///
    /// Recovers the VERBATIM value from the parent Episode `content`, NOT from
    /// the lossy chunk `text` (the markdown chunker's smart-punctuation pass
    /// rewrites quotes / dashes and corrupts JSON values — see
    /// [`Self::recover_value`]).
    pub async fn read(&self, k: &str) -> Result<Option<serde_json::Value>, LunarisError> {
        let filter = Filter::Eq {
            field: "source".into(),
            value: serde_json::Value::String(self.scope_key(k)),
        };
        match self.find(k, filter).await?.into_iter().next() {
            Some(h) => self.recover_value(&h.episode_id).await,
            None => Ok(None),
        }
    }

    /// Return all `(source, value)` pairs whose `source` starts with
    /// `{scope_prefix}{pattern}`. Values are recovered verbatim from the
    /// parent Episode `content` (see [`Self::recover_value`]).
    pub async fn grep(
        &self,
        pattern: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, LunarisError> {
        let filter = Filter::StartsWith { field: "source".into(), prefix: self.scope_key(pattern) };
        let hits = self.find(pattern, filter).await?;
        // A value large enough to chunk-split yields multiple hits sharing one
        // parent `episode_id` (and `source`). Recover each DISTINCT episode
        // exactly once, in rank order, so grep returns one entry per key rather
        // than one per chunk — and never re-reads the same Episode KV row.
        // Without this, a single large value crowds the whole `top_k` window
        // with identical entries and can starve out distinct sibling keys.
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            if !seen.insert(h.episode_id.clone()) {
                continue;
            }
            if let Some(v) = self.recover_value(&h.episode_id).await? {
                out.push((h.source, v));
            }
        }
        Ok(out)
    }

    /// Locate scratchpad hits via the fused Vector+Keyword(BM25) plan, falling
    /// back to vector-only when the backend's `keyword_search` is
    /// `NotSupported` (the embedded / sqlite backend — the `lunaris-mcp`
    /// default — has no FTS5 BM25 yet). This is the SINGLE find path: callers
    /// MUST NOT re-implement a second fallback (the `Filter` is enforced at the
    /// SQL boundary on the vector branch, so exact-key / prefix scoping holds).
    async fn find(&self, query: &str, filter: Filter) -> Result<Vec<Hit>, LunarisError> {
        let fused = Vector::new("chunks", DEFAULT_TOP_K * FANOUT)
            .and(Keyword::bm25("chunks", DEFAULT_TOP_K * FANOUT))
            .fuse_rrf(RRF_K)
            .top(DEFAULT_TOP_K);
        // Thread the bound scope (RFC 0001) — `Lunaris::recall()` alone seeds
        // `Scope::dev()`, which would read a different partition than `write`
        // ingested into. `with_scope` is the canonical scoped-recall entry.
        match self
            .lunaris
            .recall()
            .with_scope(self.scope.clone())
            .with_root(fused)
            .filter(filter.clone())
            .execute(Query::text(query))
            .await
        {
            Ok(hits) => Ok(hits),
            Err(err) if is_keyword_not_supported(&err) => {
                self.lunaris
                    .recall()
                    .with_scope(self.scope.clone())
                    .with_root(Vector::new("chunks", DEFAULT_TOP_K * FANOUT))
                    .filter(filter)
                    .execute(Query::text(query))
                    .await
            }
            Err(err) => Err(err),
        }
    }

    /// Recover the verbatim stored value from a hit's parent Episode `content`.
    ///
    /// `WorkingMemory::write` stores the value as `serde_json::to_string(&v)`
    /// on the Episode `content` field. The chunk `text` carried on a [`Hit`] is
    /// a smart-punctuation-rewritten projection of that content (the markdown
    /// chunker runs `pulldown_cmark` with `ENABLE_SMART_PUNCTUATION`), so
    /// deserialising the value from `Hit::text` corrupts any JSON object /
    /// array. We read the Episode KV row directly and parse its `content`,
    /// which is never chunked — lossless on every backend.
    ///
    /// `episode_id` is the 16-byte parent-episode ULID carried on each hydrated
    /// hit. An empty / malformed id (hit produced outside the main hydration
    /// path) yields `None` rather than an error.
    async fn recover_value(
        &self,
        episode_id: &[u8],
    ) -> Result<Option<serde_json::Value>, LunarisError> {
        let bytes: [u8; 16] = match episode_id.try_into() {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let key = episode_key(&self.scope, Ulid::from_bytes(bytes));
        // Live snapshot — mirrors `lunaris_retrieve::hydrate`'s `as_of = None`
        // idiom (read the latest visible version without perturbing the engine
        // clock).
        let snapshot = HlcClock::new(0).tick();
        match self.lunaris.storage().read_as_of(&self.scope, &key, snapshot).await? {
            Some(row) => {
                let episode: Episode = serde_json::from_slice(&row.value)
                    .map_err(|e| LunarisError::from(StorageError::from(e)))?;
                let value = serde_json::from_str(&episode.content)
                    .map_err(|e| LunarisError::from(StorageError::from(e)))?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Phase 9.1 Plan 01 Task 3 — run one consolidation pass scoped to
    /// `self.scope_prefix`.
    pub async fn consolidate(&self) -> Result<ConsolidationReport, LunarisError> {
        let storage: Arc<dyn StoragePort> = self.lunaris.storage();

        let events = drain_consolidate_events(&storage, &self.scope).await?;

        let pipeline = self.lunaris.consolidator_pipeline();
        let consolidator = match pipeline.snapshot_consolidator() {
            Some(c) => c,
            None => {
                return Ok(ConsolidationReport::default());
            }
        };

        let report = consolidator
            .consolidate_scoped(storage.clone(), &events, Some(&self.scope_prefix))
            .await?;

        // T1b fix (260609-dvi): emit BOTH promotion AND archive audit events.
        // Replaces the promotion-only loop with publish_per_event_audits, which
        // matches the background worker's emit behavior (D-22 verbatim).
        lunaris_consolidate::publish_per_event_audits(&storage, &report).await;

        Ok(report)
    }

    fn scope_key(&self, k: &str) -> String {
        format!("{}{}", self.scope_prefix, k)
    }
}

/// `true` when `err` is the embedded/sqlite backend reporting that
/// `keyword_search` (FTS5 BM25) is `NotSupported`. Drives [`WorkingMemory::find`]'s
/// vector-only fallback. Mirrors `lunaris_mcp::tools::staging::is_keyword_not_supported`
/// — duplicated (not shared) because `lunaris` cannot depend on `lunaris-mcp`.
fn is_keyword_not_supported(err: &LunarisError) -> bool {
    matches!(
        err,
        LunarisError::Storage(StorageError::NotSupported(msg))
            if msg.contains("keyword_search") || msg.contains("keyword")
    )
}

/// Phase 9.1 Plan 01 Task 3 — drain up to [`DRAIN_CAP`] recent
/// [`ConsolidateEvent`]s from [`CONSOLIDATE_TOPIC`].
///
/// T1a fix (260609-dvi): subscribes under the caller's real `scope` rather than
/// `Scope::dev()`. Events published under the server scope are now correctly consumed.
async fn drain_consolidate_events(
    storage: &Arc<dyn StoragePort>,
    scope: &Scope,
) -> Result<Vec<ConsolidateEvent>, LunarisError> {
    let pull_timeout = Duration::from_millis(PULL_TIMEOUT_MS);

    let mut stream = storage
        .subscribe(scope, CONSOLIDATE_CONSUMER_GROUP, CONSOLIDATE_TOPIC, 0)
        .await
        .map_err(LunarisError::Storage)?;

    let mut events = Vec::with_capacity(64);
    while events.len() < DRAIN_CAP {
        match tokio::time::timeout(pull_timeout, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Ok(ev) = serde_json::from_slice::<ConsolidateEvent>(&msg.payload) {
                    events.push(ev);
                }
            }
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRIM-04 ≤ 30 LOC public-surface contract.
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
            "PRIM-04 contract: WorkingMemory needs at least 3 public methods; got {pub_fns}"
        );
    }

    #[test]
    fn working_memory_scope_key_prefix_concatenation() {
        fn scope_key(prefix: &str, k: &str) -> String {
            format!("{prefix}{k}")
        }
        assert_eq!(scope_key("helios:fs/", "note-1"), "helios:fs/note-1");
        assert_eq!(scope_key("chat:user-42/", "draft"), "chat:user-42/draft");
        assert_eq!(scope_key("", "raw-key"), "raw-key");
    }

    #[test]
    fn working_memory_grep_uses_starts_with_filter() {
        let prefix = "chat:user-42/draft-";
        let filter = Filter::StartsWith { field: "source".into(), prefix: prefix.into() };
        match filter {
            Filter::StartsWith { field, prefix: p } => {
                assert_eq!(field, "source");
                assert_eq!(p, "chat:user-42/draft-");
            }
            other => panic!("expected StartsWith variant; got {other:?}"),
        }
    }

    #[test]
    fn working_memory_construction_records_scope() {
        let s = format!("{}{}", "helios:fs/", "k");
        assert!(s.starts_with("helios:fs/"));
        assert!(s.ends_with("k"));
        assert_eq!(s, "helios:fs/k");
    }
}
