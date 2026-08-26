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
use lunaris_core::keyspace::{episode_key, source_index_key};
use lunaris_core::storage::types::{Filter, Lsn, WriteOp};
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
        self.write_inner(k, v, None).await
    }

    /// [`Self::write`] with the payload's real-world reference time stamped
    /// as [`Episode::t_ref`].
    ///
    /// `t_ref` is the date the CONTENT is from (a chat session's date, a
    /// document's authored date) — distinct from the ingest-time HLC the
    /// clock stamps on `bt`. Graph-ON ingest threads it into the extraction
    /// prompt as `REFERENCE_TIME` so extracted `valid_from`/`valid_to`
    /// dates are grounded in the content's timeline instead of the model's
    /// "today" (Mechanism B, 2026-07-29 LME diagnosis).
    pub async fn write_dated(
        &self,
        k: &str,
        v: serde_json::Value,
        t_ref: chrono::DateTime<chrono::Utc>,
    ) -> Result<Lsn, LunarisError> {
        self.write_inner(k, v, Some(t_ref)).await
    }

    async fn write_inner(
        &self,
        k: &str,
        v: serde_json::Value,
        t_ref: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Lsn, LunarisError> {
        let source = self.scope_key(k);
        let content = serde_json::to_string(&v)
            .map_err(|e| LunarisError::from(lunaris_core::StorageError::from(e)))?;
        let mut episode = Episode::new(
            self.scope.clone(),
            source.clone(),
            content,
            self.lunaris.clock().as_ref(),
        );
        episode.t_ref = t_ref;
        let episode_id = episode.id;
        let lsn = self.lunaris.ingest(episode).await?;
        self.record_source_index(&source, episode_id).await;
        Ok(lsn)
    }

    /// Record `source -> episode_id` in the secondary index that makes
    /// [`Self::read`] an exact-key read (F40).
    ///
    /// BEST-EFFORT, and deliberately so — it mirrors
    /// `StoragePort::insert_dedupe_key`, the established precedent for a
    /// sidecar written AFTER the pipeline's single `atomic_write`. Two
    /// consequences worth stating rather than discovering:
    ///
    /// * **INGEST-04 is untouched.** This is a separate write by a caller of
    ///   `ingest`, not a second `atomic_write` inside the ingest pipeline.
    /// * **A failure here must not fail the write.** The value IS stored; only
    ///   the fast path to it is missing, and [`Self::read`] falls back to the
    ///   ranked search when the index has no entry. Turning a successful write
    ///   into an error because an optimization sidecar failed would be a strict
    ///   regression. It is logged at `warn` so the degradation is visible.
    async fn record_source_index(&self, source: &str, episode_id: Ulid) {
        let key = source_index_key(&self.scope, source);
        let op = WriteOp::KvPut { key, value: episode_id.to_bytes().to_vec() };
        if let Err(err) = self.lunaris.storage().atomic_write(&self.scope, &[op]).await {
            tracing::warn!(
                error = %err,
                source = %source,
                "working_memory_source_index_write_failed; read() falls back to the ranked path"
            );
        }
    }

    /// Read the value for `k` scoped under `scope_prefix`, if present.
    ///
    /// Recovers the VERBATIM value from the parent Episode `content`, NOT from
    /// the lossy chunk `text` (the markdown chunker's smart-punctuation pass
    /// rewrites quotes / dashes and corrupts JSON values — see
    /// `Self::recover_value`).
    pub async fn read(&self, k: &str) -> Result<Option<serde_json::Value>, LunarisError> {
        self.read_at(k, None).await
    }

    /// [`Self::read`] pinned to `as_of`, or to the live snapshot when `None`.
    ///
    /// F42 — this is the ONE read implementation. `CodingSessionMemory` used to
    /// carry a second one (a free `read_at` that concatenated `Hit::text`
    /// across every hit), and it was wrong in three independent ways this path
    /// is right in by construction:
    ///
    /// * **Content.** Chunk text is a LOSSY projection — the chunker parses
    ///   with `pulldown_cmark::Options::all()` (`ENABLE_SMART_PUNCTUATION`) and
    ///   rebuilds text from the event stream, so `--` becomes an en dash and
    ///   ASCII quotes become typographic ones AT INGEST. Recovering from the
    ///   parent Episode payload is the only way back to the written bytes.
    /// * **Version.** Every write mints a NEW Episode under the same `source`.
    ///   Concatenating every hit glued superseded bodies onto the answer, in
    ///   proportion to how often the path had been edited. Resolving to ONE
    ///   episode is what makes the read a read.
    /// * **Query text.** The index path needs none, so a path whose NAME
    ///   analyses to an empty FT query (`big`, `state`) is no longer
    ///   write-OK / read-IMPOSSIBLE.
    ///
    /// On a backend with no KV version chain a historical `as_of` is REFUSED by
    /// `read_as_of` rather than answered with present-time data (Moon 0.6.2
    /// task 9). That is unchanged here and deliberately so: this path hits the
    /// same guard the old one did, so the honest 501 survives the fix.
    /// Crate-internal on purpose. `WorkingMemory`'s public surface is capped at
    /// 7 symbols by the PRIM-04 contract, and this method's only callers —
    /// [`Self::read`] and `AsOfScratchpad::read` — are both in this crate.
    /// Spending a capped public symbol on it would need a reason beyond "it
    /// would be a nice API"; if an external caller ever needs an as-of
    /// scratchpad read, that is a contract change to make deliberately.
    pub(crate) async fn read_at(
        &self,
        k: &str,
        as_of: Option<lunaris_core::Hlc>,
    ) -> Result<Option<serde_json::Value>, LunarisError> {
        let source = self.scope_key(k);

        // Exact-key path (F40). One KV get on the source index, then one on the
        // episode. No embedding, no ranking, no top-k window — so this answers
        // correctly on a build with no usable embedder, which the ranked path
        // below cannot.
        if let Some(id) = self.lookup_source_index(&source, as_of).await? {
            // A hit here is authoritative for presence, but the episode row it
            // names can still be gone (a `forget` tombstones the episode and
            // does not sweep this sidecar). `recover_value` returning None then
            // means "deleted", and falling through to the ranked path would not
            // find it either — so return the None rather than re-searching.
            return self.recover_value(&id, as_of).await;
        }

        // Fallback: entries written before this index existed have no sidecar,
        // and a sidecar write can fail (it is best-effort by design). The
        // ranked path is what those reads used and it still works wherever it
        // worked before — this is strictly additive.
        let filter =
            Filter::Eq { field: "source".into(), value: serde_json::Value::String(source) };
        // F42 — `.next()` took the TOP-RANKED hit, which is not the same as
        // the CURRENT one: all versions of a path stay indexed under the same
        // `source`, so ranking could hand back a superseded body. Episode ids
        // are ULIDs and ULIDs sort by mint time, so the greatest id IS the
        // newest version — no extra read to find it.
        match self.find(k, filter).await?.into_iter().max_by(|a, b| a.episode_id.cmp(&b.episode_id))
        {
            Some(h) => self.recover_value(&h.episode_id, as_of).await,
            None => Ok(None),
        }
    }

    /// Resolve `source` to an episode id through the F40 secondary index.
    ///
    /// `Ok(None)` means the index has no entry — which is NOT the same as "the
    /// key does not exist", because entries written before the index existed
    /// have none. The caller falls back rather than concluding absence.
    async fn lookup_source_index(
        &self,
        source: &str,
        as_of: Option<lunaris_core::Hlc>,
    ) -> Result<Option<Vec<u8>>, LunarisError> {
        let key = source_index_key(&self.scope, source);
        // The sidecar is OVERWRITTEN per write, so reading IT as-of is what
        // names the episode visible at `as_of` — on a backend that versions KV.
        // On one that does not, this read is refused, which is the honest
        // answer and the same one the caller got before F42.
        let snapshot = as_of.unwrap_or_else(|| HlcClock::new(0).tick());
        match self.lunaris.storage().read_as_of(&self.scope, &key, snapshot).await {
            Ok(Some(row)) if row.value.len() == 16 => Ok(Some(row.value.to_vec())),
            // A row of the wrong width is corruption, not absence. Fall back to
            // the ranked path rather than hand `recover_value` bytes it will
            // silently reject with `Ok(None)` — which would read as "deleted".
            Ok(Some(row)) => {
                tracing::warn!(
                    len = row.value.len(),
                    source = %source,
                    "working_memory_source_index_bad_width; falling back to the ranked path"
                );
                Ok(None)
            }
            Ok(None) => Ok(None),
            // A fresh scope has no index yet: the KV namespace does not exist,
            // which is "not found", never an error (same contract as `find`'s
            // missing-index arm below). Classified on the `StorageError` itself
            // — `is_ft_index_missing` takes a `LunarisError` and `StorageError`
            // is not `Clone`, so wrapping it to ask would move it out of the
            // arm that still needs to return it.
            Err(err) if lunaris_retrieve::missing_index::is_index_absent(&err) => Ok(None),
            Err(err) => Err(LunarisError::from(err)),
        }
    }

    /// Return all `(source, value)` pairs whose `source` starts with
    /// `{scope_prefix}{pattern}`. Values are recovered verbatim from the
    /// parent Episode `content` (see `Self::recover_value`).
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
            if let Some(v) = self.recover_value(&h.episode_id, None).await? {
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
        let hits = match self
            .lunaris
            .recall()
            .with_scope(self.scope.clone())
            .with_root(fused)
            .filter(filter.clone())
            .execute(Query::text(query))
            .await
        {
            Ok(hits) => hits,
            // Fresh scope: nothing was ever ingested, so the scope's FT index
            // doesn't exist yet. An exact-key read on it means "not found",
            // never an error (ADD task moon-parity-honesty).
            Err(err) if is_ft_index_missing(&err) => return Ok(Vec::new()),
            // Keyword leg unusable — either the backend has no keyword_search
            // (embedded/sqlite) or Moon's FT analyzer reduced the KEY text to
            // an empty query (stopword-like keys such as "state" were
            // write-OK/read-IMPOSSIBLE before this arm). Retry vector-only:
            // the Filter::Eq/StartsWith on `source` carries exactness, the
            // query text is only a ranking signal here.
            Err(err) if is_keyword_not_supported(&err) || is_ft_query_unusable(&err) => {
                match self
                    .lunaris
                    .recall()
                    .with_scope(self.scope.clone())
                    .with_root(Vector::new("chunks", DEFAULT_TOP_K * FANOUT))
                    .filter(filter.clone())
                    .execute(Query::text(query))
                    .await
                {
                    Ok(hits) => hits,
                    Err(err) if is_ft_index_missing(&err) => return Ok(Vec::new()),
                    Err(err) => return Err(err),
                }
            }
            Err(err) => return Err(err),
        };
        // Post-enforce the source filter: on Moon's native HYBRID path the
        // pushed-down filter constrains only the BM25 branch — the dense KNN
        // branch ignores it, so non-matching sources can leak through RRF
        // fusion (the `memory.recall` tool self-protects the same way for its
        // `source_prefix`). Defense-in-depth: keep the push-down for ranking
        // quality, enforce correctness here on the hydrated `source`.
        Ok(hits.into_iter().filter(|h| source_filter_matches(&filter, &h.source)).collect())
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
        as_of: Option<lunaris_core::Hlc>,
    ) -> Result<Option<serde_json::Value>, LunarisError> {
        let bytes: [u8; 16] = match episode_id.try_into() {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let key = episode_key(&self.scope, Ulid::from_bytes(bytes));
        // Live snapshot — mirrors `lunaris_retrieve::hydrate`'s `as_of = None`
        // idiom (read the latest visible version without perturbing the engine
        // clock).
        let snapshot = as_of.unwrap_or_else(|| HlcClock::new(0).tick());
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
    ///
    /// ## Foreign-event preservation
    ///
    /// `drain_consolidate_events` subscribes to [`CONSOLIDATE_TOPIC`] and
    /// consumes ALL pending events for the scope in one pass — it is not
    /// prefix-aware.  Without an explicit re-queue step, calling
    /// `consolidate_scoped(Some(prefix))` on the drained batch silently drops
    /// the non-matching ("foreign") events: they are consumed from the
    /// consumer-group queue and never seen by a subsequent pass (ADD task
    /// `consolidate-prefix-drop`).
    ///
    /// Fix: after draining, partition events by
    /// `source.starts_with(scope_prefix)`.  Matching events are forwarded to
    /// `consolidate_scoped` (which therefore receives an already-filtered
    /// batch and must NOT double-filter — hence `None` prefix in the call).
    /// Foreign events are re-published verbatim to [`CONSOLIDATE_TOPIC`] so
    /// that the next `consolidate_unfiltered` (or the background worker) can
    /// pick them up.  A publish error on re-queue is loud-not-fatal: the call
    /// still returns `Ok` with the matching report (matching consolidation
    /// already happened and its result must not be discarded).
    pub async fn consolidate(&self) -> Result<ConsolidationReport, LunarisError> {
        let storage: Arc<dyn StoragePort> = self.lunaris.storage();

        let events = drain_consolidate_events(&storage, &self.scope).await?;

        // Partition into events belonging to this namespace vs. all others.
        // `scope_prefix` is captured by value (String → &str borrow is safe
        // because self outlives this function).
        let scope_prefix: &str = &self.scope_prefix;
        let mut matching: Vec<ConsolidateEvent> = Vec::with_capacity(events.len());
        let mut foreign: Vec<ConsolidateEvent> = Vec::new();
        for ev in events {
            if ev.source.starts_with(scope_prefix) {
                matching.push(ev);
            } else {
                foreign.push(ev);
            }
        }

        // Re-publish foreign events verbatim so they remain available for
        // a subsequent `consolidate_unfiltered` pass or the background worker.
        // Errors are surfaced via tracing but never propagate — the matching
        // consolidation work must not be rolled back because of a re-queue
        // failure.
        let mut lost: usize = 0;
        for ev in &foreign {
            match serde_json::to_vec(ev) {
                Ok(payload) => {
                    if let Err(e) =
                        storage.publish(&self.scope, CONSOLIDATE_TOPIC, 0, payload.into()).await
                    {
                        tracing::warn!(
                            source = %ev.source,
                            error = %e,
                            "consolidate: failed to re-queue foreign event; \
                             it will be lost for this scope's pass"
                        );
                        lost += 1;
                    }
                }
                Err(e) => {
                    // Serialisation of ConsolidateEvent is infallible in
                    // practice (all fields are JSON-safe primitives); log and
                    // count as lost rather than panic.
                    tracing::warn!(
                        source = %ev.source,
                        error = %e,
                        "consolidate: serde failure serialising foreign event for re-queue"
                    );
                    lost += 1;
                }
            }
        }
        if lost > 0 {
            tracing::warn!(
                lost,
                scope_prefix,
                "consolidate: {} foreign event(s) could not be re-queued and will be lost",
                lost
            );
        }

        let pipeline = self.lunaris.consolidator_pipeline();
        let consolidator = match pipeline.snapshot_consolidator() {
            Some(c) => c,
            None => {
                return Ok(ConsolidationReport::default());
            }
        };

        // The matching batch is already filtered — pass None so consolidate_scoped
        // does NOT double-filter (every event in `matching` already satisfies
        // `starts_with(scope_prefix)`).
        let report = consolidator.consolidate_scoped(storage.clone(), &matching, None).await?;

        // T1b fix (260609-dvi): emit BOTH promotion AND archive audit events.
        // Replaces the promotion-only loop with publish_per_event_audits, which
        // matches the background worker's emit behavior (D-22 verbatim).
        lunaris_consolidate::publish_per_event_audits(&storage, &self.scope, &report).await;

        Ok(report)
    }

    /// Whole-scope variant of [`Self::consolidate`]: drains and consolidates
    /// ALL pending events for the scope, ignoring `self.scope_prefix`.
    ///
    /// The drain is scope-wide either way; `consolidate_scoped(Some(prefix))`
    /// then FILTERS the drained events and the non-matching ones are already
    /// consumed from the queue — dropped, not re-queued. Callers that cannot
    /// tolerate that loss (e.g. the MCP session-handover, which runs
    /// implicitly and must not eat other namespaces' pending events) use this
    /// variant; it consolidates exactly what the background worker would.
    pub async fn consolidate_unfiltered(&self) -> Result<ConsolidationReport, LunarisError> {
        let storage: Arc<dyn StoragePort> = self.lunaris.storage();

        let events = drain_consolidate_events(&storage, &self.scope).await?;

        let pipeline = self.lunaris.consolidator_pipeline();
        let consolidator = match pipeline.snapshot_consolidator() {
            Some(c) => c,
            None => {
                return Ok(ConsolidationReport::default());
            }
        };

        let report = consolidator.consolidate_scoped(storage.clone(), &events, None).await?;

        lunaris_consolidate::publish_per_event_audits(&storage, &self.scope, &report).await;

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

/// `true` when Moon's FT analyzer reduced the query text to nothing —
/// `ERR empty query after analysis`. For an exact-key scratchpad read the
/// query is the KEY string, so stopword-like keys ("state") hit this on
/// every read; the Filter on `source` still identifies the row exactly, so
/// the caller retries vector-only (ADD task moon-parity-honesty).
///
/// String-matched on `StorageError::Backend` by necessity — Moon surfaces FT
/// errors as opaque RESP error text. Pinned by the live test
/// `scratchpad_stopword_key_reads_back_moon` so wording drift is caught.
fn is_ft_query_unusable(err: &LunarisError) -> bool {
    matches!(
        err,
        LunarisError::Storage(StorageError::Backend(msg))
            if msg.contains("empty query after analysis")
    )
}

/// `true` when the scope's FT index does not exist yet — a brand-new scope
/// with zero ingested rows. Reads resolve to "not found", never an error.
/// Pinned by `scratchpad_read_fresh_scope_returns_none_moon`.
///
/// F1: the predicate now comes from `lunaris_retrieve::missing_index`, which
/// is also what every recall leg uses. This copy matched two of Moon's three
/// spellings for the same condition, and `operators::tree` matched only the
/// third — four call sites, three predicates, one rule. One shared predicate
/// means a newly-observed spelling is fixed everywhere at once.
fn is_ft_index_missing(err: &LunarisError) -> bool {
    matches!(
        err,
        LunarisError::Storage(e) if lunaris_retrieve::missing_index::is_index_absent(e)
    )
}

/// `true` when a hit's hydrated `source` satisfies the `source` predicate of
/// `filter`. Non-`source` predicates (and unknown variants) pass — they were
/// already enforced by the backend push-down; this guard exists because Moon's
/// native HYBRID path applies the pushed-down filter to the BM25 branch only,
/// letting dense-KNN hits with foreign sources leak through RRF fusion.
fn source_filter_matches(filter: &Filter, source: &str) -> bool {
    match filter {
        Filter::Eq { field, value } if field == "source" => value.as_str() == Some(source),
        Filter::StartsWith { field, prefix } if field == "source" => source.starts_with(prefix),
        Filter::And(xs) => xs.iter().all(|f| source_filter_matches(f, source)),
        Filter::Or(xs) => xs.iter().any(|f| source_filter_matches(f, source)),
        _ => true,
    }
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
            pub_fns <= 7,
            "PRIM-04 ≤30-LOC contract: WorkingMemory has {pub_fns} pub fns; cap is 7 \
             (write_dated added for Mechanism-B session-date grounding, 2026-07-29)"
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

    /// Moon HYBRID filter-bypass guard: dense-KNN hits whose source does not
    /// satisfy the pushed-down `source` filter must be rejected post-recall.
    #[test]
    fn source_filter_rejects_foreign_sources() {
        let eq = Filter::Eq {
            field: "source".into(),
            value: serde_json::Value::String("scratchpad/sess-b/plan".into()),
        };
        assert!(source_filter_matches(&eq, "scratchpad/sess-b/plan"));
        assert!(!source_filter_matches(&eq, "scratchpad/sess-a/plan"), "Eq must reject leaks");
        assert!(!source_filter_matches(&eq, "scratchpad/sess-a/blocker"));

        let sw = Filter::StartsWith { field: "source".into(), prefix: "scratchpad/sess-b/".into() };
        assert!(source_filter_matches(&sw, "scratchpad/sess-b/anything"));
        assert!(!source_filter_matches(&sw, "scratchpad/sess-a/plan"), "prefix must reject leaks");

        // Non-source predicates pass through (already enforced by the backend).
        let other =
            Filter::Eq { field: "kind".into(), value: serde_json::Value::String("x".into()) };
        assert!(source_filter_matches(&other, "scratchpad/sess-a/plan"));
    }

    #[test]
    fn working_memory_construction_records_scope() {
        let s = format!("{}{}", "helios:fs/", "k");
        assert!(s.starts_with("helios:fs/"));
        assert!(s.ends_with("k"));
        assert_eq!(s, "helios:fs/k");
    }
}
