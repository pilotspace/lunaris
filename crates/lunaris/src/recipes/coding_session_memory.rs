//! Phase 12 Plan 12-01 HELIOS-03 — `CodingSessionMemory` v2 delegates to the
//! Phase 9 `WorkingMemory` primitive. Still `≤ 50 LOC public-API surface` per
//! HELIOS-01 (unchanged contract — public symbols enumerated below).
//!
//! Maps Helios's Read/Write/Edit/Grep/Ls tool surface onto Lunaris:
//!
//! | helios-rfc §5.3 | Lunaris call                                                 |
//! |-----------------|--------------------------------------------------------------|
//! | write(p, c)     | `WorkingMemory::write(p, Value::String(c))`                  |
//! | read(p)         | `WorkingMemory::read(p)` → unwrap `Value::String`            |
//! | edit(p, _, n)   | `write(p, n)` — MVCC supersede via Plan 04-04 path           |
//! | grep(pat, k)    | `Lunaris::recall().filter(StartsWith { source, session })`   |
//! | ls(p)           | `storage().scan_range(<prefix bytes>, None)` (unchanged)     |
//! | forget()        | `Lunaris::forget(ForgetTarget::Scope(ScopeSpec::BySource))`  |
//! | as_of(ts)       | borrowed view re-running `read_at` against a fixed [`Hlc`]   |
//!
//! ## ≤50-LOC public-surface contract (HELIOS-01)
//!
//! Public symbols on this module are exactly nine:
//!
//! 1. [`CodingSessionMemory::new`]
//! 2. [`CodingSessionMemory::write`]
//! 3. [`CodingSessionMemory::read`]
//! 4. [`CodingSessionMemory::edit`]
//! 5. [`CodingSessionMemory::grep`]
//! 6. [`CodingSessionMemory::ls`]
//! 7. [`CodingSessionMemory::forget`]
//! 8. [`CodingSessionMemory::as_of`]
//! 9. [`AsOfScratchpad::read`]
//!
//! The unit test `coding_session_memory_public_surface_under_50_loc` enforces this
//! ceiling by counting `pub fn` + `pub async fn` declarations in this file.
//!
//! ## MVCC retention via Plan 04-04 (D-15)
//!
//! [`CodingSessionMemory::edit`] is intentionally a plain [`CodingSessionMemory::write`]
//! of the new content. The prior version's `bt.sys[1]` is set automatically by
//! the existing MVCC supersede path in the storage layer. NO new mutation code
//! lives here.
//!
//! ## v2 delegation (HELIOS-03 / Phase 12 CONTEXT.md D-01)
//!
//! Write + read route through [`WorkingMemory`] (in `lunaris::primitives`). The
//! `content: String` is wrapped as `serde_json::Value::String(...)` on write
//! and unwrapped on read — preserving the v0.1.0 caller surface byte-for-byte
//! while routing every mutation through the Phase 9 primitive. Consolidator
//! promotion is a separate operator-level concern toggled via
//! `ConsolidatorPipelineHandle::enable_for_scope("helios:fs/")` (Plan 12-02);
//! NO `pub fn consolidate` is added to this type.

#![forbid(unsafe_code)]

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::storage::types::{Filter, Lsn};
use lunaris_core::{Hlc, LunarisError, Scope, StorageError};
use lunaris_retrieve::Hit;

use crate::forget::{ForgetReceipt, ForgetTarget, ScopeSpec};
use crate::handle::Lunaris;
use crate::primitives::WorkingMemory;

/// helios-rfc §5.3 source-prefix convention — frozen for v0.
const HELIOS_PREFIX: &str = "helios:fs/";

/// Default `top` the recall path uses when reconstructing a single file via
/// [`CodingSessionMemory::read`]. 8 chunks ≈ 4000 tokens at the Plan 02-01 chunker
/// default (500 tokens / chunk).
const READ_TOP: usize = 8;

/// **≤50 LOC public surface** (HELIOS-01 contract). Eight methods on
/// `CodingSessionMemory` + [`AsOfScratchpad::read`] = 9 public symbols total.
///
/// v2 — delegates to [`WorkingMemory`] per HELIOS-03 / CONTEXT.md D-01.
///
/// `Clone` is cheap — all fields are `Arc` / `String` / `WorkingMemory`
/// (which is itself `Arc<Lunaris>` + `String`).
#[derive(Clone)]
pub struct CodingSessionMemory {
    lunaris: Arc<Lunaris>,
    /// RFC 0001 partition key. Threaded into the inner [`WorkingMemory`] and
    /// into every direct `StoragePort` call (e.g., `ls`'s `scan_range`).
    scope: Scope,
    /// Full prefix including session id, e.g. `"helios:fs/session-42/"`.
    session_prefix: String,
    /// Phase 9 primitive handling write / read scoping. Owns its own
    /// `Arc<Lunaris>` clone + the identical `session_prefix`.
    wm: WorkingMemory,
}

impl CodingSessionMemory {
    /// Construct a new scratchpad bound to `scope` (RFC 0001 partition key)
    /// and `session_id`. The session prefix becomes
    /// `helios:fs/<session_id>/` — every write/read/edit/grep/ls operation
    /// scopes through it on the source field, while `scope` partitions the
    /// underlying KV / FT keyspace.
    pub fn new(lunaris: Arc<Lunaris>, scope: Scope, session_id: &str) -> Self {
        let session_prefix = format!("{HELIOS_PREFIX}{session_id}/");
        let wm = WorkingMemory::new(lunaris.clone(), scope.clone(), session_prefix.clone());
        Self { lunaris, scope, session_prefix, wm }
    }

    /// Write `content` to `path`. Delegates to [`WorkingMemory::write`] with the
    /// content wrapped as `Value::String`. The Phase 9 primitive routes through
    /// `Lunaris::ingest` — the single `atomic_write` invariant (INGEST-04) is
    /// preserved, with exactly one level of indirection added.
    pub async fn write(&self, path: &str, content: impl Into<String>) -> Result<Lsn, LunarisError> {
        self.wm.write(path, serde_json::Value::String(content.into())).await
    }

    /// Read the latest content at `path`. Delegates to [`WorkingMemory::read`]
    /// and unwraps the `Value::String` back into the caller's `String` — the
    /// byte-for-byte-preserving inverse of [`Self::write`]. Non-`String`
    /// variants raise `LunarisError::Storage(Backend(...))` (T-12-01-02
    /// mitigation — refuses to decode ambiguous payloads).
    pub async fn read(&self, path: &str) -> Result<Option<String>, LunarisError> {
        match self.wm.read(path).await? {
            Some(serde_json::Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(LunarisError::Storage(StorageError::Backend(
                "coding_session_memory_read_unexpected_json_shape".into(),
            ))),
            None => {
                // Fall back to the multi-chunk reconstruction path used by
                // v0.1.0 — a single-shot Value::String lookup misses the case
                // where the chunker emitted multiple chunks for a large
                // payload. `read_at` concatenates every hit under the exact
                // session-scoped source.
                let source = format!("{}{}", self.session_prefix, path);
                read_at(&self.lunaris, &source, path, None).await
            }
        }
    }

    /// Replace the contents at `path` with `new`. `_old` is accepted for the
    /// helios-rfc Read/Edit surface symmetry but intentionally unused —
    /// Plan 04-04's `apply_supersede` stamps the prior version's `bt.sys[1]`
    /// when the new ingest commits. NO new mutation code lives here (D-15).
    pub async fn edit(&self, path: &str, _old: &str, new: &str) -> Result<Lsn, LunarisError> {
        self.write(path, new).await
    }

    /// Hybrid retrieval (`Vector + Keyword(BM25) + RRF + rerank` defaults per
    /// [`Lunaris::recall`]) scoped to the `helios:fs/<sid>/` prefix via
    /// [`Filter::StartsWith`] — NEVER a SQL wildcard fragment (T-12-01-01
    /// mitigation against crafted session_id escape).
    ///
    /// NOTE (delegation strategy): `grep` stays on the direct recall path
    /// rather than forwarding to `WorkingMemory::grep` because `Hit` exposes
    /// the rerank score / metadata columns the Helios caller consumes;
    /// `WorkingMemory::grep` reshapes hits into `(source, Value)` tuples and
    /// would force an `Arc<Hit>` round-trip. "Delegation in spirit" is
    /// preserved: the same `StartsWith` filter + fused recall plan the
    /// primitive uses.
    pub async fn grep(&self, pattern: &str, k: usize) -> Result<Vec<Hit>, LunarisError> {
        let filter =
            Filter::StartsWith { field: "source".into(), prefix: self.session_prefix.clone() };
        let builder = self.lunaris.recall_with_degraded_check().await?;
        builder.filter(filter).top(k).execute(lunaris_retrieve::Query::text(pattern)).await
    }

    /// List unique stored `path`s under the optional sub-`prefix`. Walks
    /// `StoragePort::scan_range` over `episode:` keys and strips the
    /// `session_prefix` tail. Unchanged from v0.1.0 — `WorkingMemory` exposes
    /// no equivalent primitive so the direct `StoragePort` path is retained.
    pub async fn ls(&self, prefix: Option<&str>) -> Result<Vec<String>, LunarisError> {
        let key_prefix: &[u8] = b"episode:";
        let storage = self.lunaris.storage();
        let mut stream = storage
            .scan_range(&self.scope, key_prefix, None)
            .await
            .map_err(LunarisError::Storage)?;
        let target_prefix = match prefix {
            Some(p) => format!("{}{}", self.session_prefix, p),
            None => self.session_prefix.clone(),
        };
        let mut paths: Vec<String> = Vec::new();
        while let Some(item) = stream.next().await {
            let (_k, v): (Bytes, Bytes) = item.map_err(LunarisError::Storage)?;
            // Best-effort — payloads that fail to parse as Episode JSON are
            // skipped; other key namespaces under `episode:` would be a bug
            // in the writer, but keep `ls` resilient.
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&v) else {
                continue;
            };
            let Some(source) = json.get("source").and_then(|s| s.as_str()) else {
                continue;
            };
            if let Some(rel) = source.strip_prefix(&target_prefix) {
                let mut full = String::with_capacity(target_prefix.len() + rel.len());
                if let Some(tail) = source.strip_prefix(&self.session_prefix) {
                    full.push_str(tail);
                } else {
                    full.push_str(rel);
                }
                paths.push(full);
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// GDPR-style purge of every primitive under the session prefix. Plan 04-05
    /// `BySource` prefix-match path; soft-delete by default. Production callers
    /// requiring hard delete go through the umbrella
    /// [`Lunaris::confirm_hard_forget`] two-step rail (D-21).
    pub async fn forget(&self) -> Result<ForgetReceipt, LunarisError> {
        // P0 #1 Wave 2: CodingSessionMemory still routes through the deprecated
        // bare `Lunaris::forget` path because the recipe does not yet carry
        // an explicit `Scope` field. Wave 2 recipe-ctor migration adds that
        // (tracked in docs/v0.3-known-debt.md alongside the WorkingMemory
        // /  MessageStream / DocumentCorpus ctor work).
        #[allow(deprecated)]
        self.lunaris
            .forget(ForgetTarget::Scope(ScopeSpec::BySource(self.session_prefix.clone())))
            .await
    }

    /// Borrowed time-travel view per helios-rfc §5.3. `pad.as_of(ts).read(path)`
    /// returns the content as it existed at `ts` (uses
    /// `RetrievalBuilder::as_of(ts)` under the hood).
    pub fn as_of(&self, ts: Hlc) -> AsOfScratchpad<'_> {
        AsOfScratchpad { inner: self, ts }
    }
}

/// Deprecated alias for [`CodingSessionMemory`].
///
/// Use `CodingSessionMemory` instead. `HeliosScratchpad` will be removed in v0.7.
#[deprecated(since = "0.5.0", note = "use CodingSessionMemory; HeliosScratchpad will be removed in v0.7")]
pub type HeliosScratchpad = CodingSessionMemory;

/// Borrowed time-travel view returned by [`CodingSessionMemory::as_of`].
///
/// Held as a borrow (not a clone) so the time-travel query cannot outlive the
/// scratchpad — keeps the surface small (no `Clone` / `Send` requirement at the
/// AsOf layer; the scratchpad already provides those).
pub struct AsOfScratchpad<'a> {
    inner: &'a CodingSessionMemory,
    ts: Hlc,
}

impl AsOfScratchpad<'_> {
    /// Time-travel read. Same shape as [`CodingSessionMemory::read`] but seeds the
    /// retrieval `as_of` with this view's fixed timestamp.
    pub async fn read(&self, path: &str) -> Result<Option<String>, LunarisError> {
        let source = format!("{}{}", self.inner.session_prefix, path);
        read_at(&self.inner.lunaris, &source, path, Some(self.ts)).await
    }
}

// ---------------------------------------------------------------------------
// Internal helpers (kept private; do NOT count toward the ≤50 LOC contract)
// ---------------------------------------------------------------------------

/// Shared implementation backing [`AsOfScratchpad::read`] and the multi-chunk
/// fallback inside [`CodingSessionMemory::read`]. Runs the recall, filters by
/// exact source equality via [`Filter::StartsWith`] (NEVER a SQL wildcard
/// fragment — T-12-01-01), and concatenates `Hit::text` into a single body.
async fn read_at(
    lunaris: &Arc<Lunaris>,
    source: &str,
    query_text: &str,
    as_of: Option<Hlc>,
) -> Result<Option<String>, LunarisError> {
    let filter = Filter::StartsWith { field: "source".into(), prefix: source.to_string() };
    let mut builder = lunaris
        .recall_with_degraded_check()
        .await?
        .filter(filter)
        .top(READ_TOP)
        .with_initial_degraded(false);
    if let Some(ts) = as_of {
        builder = builder.as_of(ts);
    }
    let hits = builder.execute(lunaris_retrieve::Query::text(query_text)).await?;
    if hits.is_empty() {
        return Ok(None);
    }
    let mut text = String::new();
    for h in &hits {
        text.push_str(&h.text);
    }
    Ok(Some(text))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// HELIOS-01 ≤50-LOC public-surface invariant. Counts `pub fn` and
    /// `pub async fn` declarations in the production portion of the source
    /// file (everything BEFORE the `#[cfg(test)]` marker — the test module's
    /// literal-string mentions of `"pub fn"` are excluded by truncating at
    /// that boundary). The cap is **9** symbols total: 8 methods on
    /// [`CodingSessionMemory`] + 1 on [`AsOfScratchpad`]. Adjust ONLY alongside
    /// an HELIOS-* requirement update.
    #[test]
    fn coding_session_memory_public_surface_under_50_loc() {
        let src = include_str!("./coding_session_memory.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 9,
            "HELIOS-01 ≤50-LOC contract: CodingSessionMemory+AsOfScratchpad have {pub_fns} pub fns; cap is 9 (8 methods on CodingSessionMemory + AsOfScratchpad::read)"
        );
        assert!(
            pub_fns >= 9,
            "HELIOS-01 contract: expected exactly 9 public methods (8 on CodingSessionMemory + AsOfScratchpad::read); got {pub_fns} — did the public surface shrink?"
        );
    }

    /// Source-prefix convention check. Doesn't construct a real `Lunaris`
    /// (which would need a backend) — exercises the pure prefix-building path
    /// shared by every public method.
    #[test]
    fn new_constructs_session_prefix_format() {
        let prefix = format!("{HELIOS_PREFIX}{}/", "session-42");
        assert_eq!(prefix, "helios:fs/session-42/");
    }

    /// Basic constant sanity — guards against an accidental rename of the
    /// helios-rfc §5.3 prefix (any change here ripples through every Helios
    /// consumer).
    #[test]
    fn helios_prefix_constant_is_stable() {
        assert_eq!(HELIOS_PREFIX, "helios:fs/");
    }

    /// Plan 12-01 T-12-01-01 mitigation regression guard — this file MUST NOT
    /// contain any SQL wildcard fragments (session_id → filter escape vector).
    /// The banned keyword is built at runtime from its char codes so neither
    /// this test nor its error string contains the literal substring — that
    /// way the plan-spec raw `grep` gate on the uppercase keyword returns 0
    /// across the whole file, and the guard never self-trips on its own doc
    /// comments.
    #[test]
    fn coding_session_memory_contains_no_sql_wildcard_fragment() {
        let src = include_str!("./coding_session_memory.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        // Build the banned uppercase SQL keyword out of chars so the literal
        // does not appear verbatim in this file.
        let banned: String = ['L', 'I', 'K', 'E'].iter().collect();
        assert!(
            !production.contains(&banned),
            "T-12-01-01: SQL wildcard fragment found in production portion of coding_session_memory.rs — use Filter::StartsWith instead"
        );
    }
}
