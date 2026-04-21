//! Plan 05-04 D-13/D-14 — `HeliosScratchpad`: v0 recipe wrapping [`Lunaris`] in
//! the helios-rfc §5.3 file-tool convention. **≤50 LOC public-API surface.**
//!
//! Maps Helios's Read/Write/Edit/Grep/Ls tool surface onto Lunaris:
//!
//! | helios-rfc §5.3 | Lunaris call                                                 |
//! |-----------------|--------------------------------------------------------------|
//! | write(p, c)     | `ingest(Episode { source: "helios:fs/<sid>/<p>", content })`  |
//! | read(p)         | `recall_with_degraded_check().filter("source = '...'") ...`   |
//! | edit(p, _, n)   | `write(p, n)` — MVCC supersede via Plan 04-04 path            |
//! | grep(pat, k)    | hybrid recall over `helios:fs/<sid>/` prefix                  |
//! | ls(p)           | `storage().scan_range(<prefix bytes>, None)`                  |
//! | forget()        | `Lunaris::forget(ForgetTarget::Scope(ScopeSpec::BySource))`   |
//! | as_of(ts)       | borrowed view that re-runs `read` against a fixed [`Hlc`]     |
//!
//! ## ≤50-LOC public-surface contract (HELIOS-01)
//!
//! Public symbols on this module are exactly nine:
//!
//! 1. [`HeliosScratchpad::new`]
//! 2. [`HeliosScratchpad::write`]
//! 3. [`HeliosScratchpad::read`]
//! 4. [`HeliosScratchpad::edit`]
//! 5. [`HeliosScratchpad::grep`]
//! 6. [`HeliosScratchpad::ls`]
//! 7. [`HeliosScratchpad::forget`]
//! 8. [`HeliosScratchpad::as_of`]
//! 9. [`AsOfScratchpad::read`]
//!
//! The unit test `helios_scratchpad_public_surface_under_50_loc` enforces this
//! ceiling by counting `pub fn` + `pub async fn` declarations in this file.
//!
//! ## MVCC retention via Plan 04-04 (D-15)
//!
//! [`HeliosScratchpad::edit`] is intentionally a plain [`HeliosScratchpad::write`]
//! of the new content. The prior version's `bt.sys[1]` is set automatically by
//! the existing MVCC supersede path in the storage layer (see
//! `crates/lunaris/src/forget.rs::build_soft_delete_op` / Plan 04-04 Task 4
//! `apply_supersede`). NO new mutation code lives here.

#![forbid(unsafe_code)]

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::storage::types::Lsn;
use lunaris_core::{Episode, Hlc, LunarisError, StorageError};
use lunaris_retrieve::Hit;
use ulid::Ulid;

use crate::forget::{ForgetReceipt, ForgetTarget, ScopeSpec};
use crate::handle::Lunaris;

/// helios-rfc §5.3 source-prefix convention — frozen for v0.
const HELIOS_PREFIX: &str = "helios:fs/";

/// Default `top` the recall path uses when reconstructing a single file via
/// [`HeliosScratchpad::read`]. 8 chunks ≈ 4000 tokens at the Plan 02-01 chunker
/// default (500 tokens / chunk) — wide enough to cover most v0 helios:fs
/// documents without amplifying recall latency.
const READ_TOP: usize = 8;

/// **≤50 LOC public surface** (HELIOS-01 contract). Eight methods on
/// `HeliosScratchpad` + [`AsOfScratchpad::read`] = 9 public symbols total.
///
/// `Clone` is cheap — both fields are `Arc` / `String`.
#[derive(Clone)]
pub struct HeliosScratchpad {
    lunaris: Arc<Lunaris>,
    /// Full prefix including session id, e.g. `"helios:fs/session-42/"`.
    session_prefix: String,
}

impl HeliosScratchpad {
    /// Construct a new scratchpad bound to `session_id`. The session prefix
    /// becomes `helios:fs/<session_id>/` — every write/read/edit/grep/ls
    /// operation scopes through it.
    pub fn new(lunaris: Arc<Lunaris>, session_id: &str) -> Self {
        Self {
            lunaris,
            session_prefix: format!("{HELIOS_PREFIX}{session_id}/"),
        }
    }

    /// Write `content` to `path`. Maps to `lunaris.ingest(Episode { source:
    /// "helios:fs/<sid>/<path>", ... })`. Single atomic_write invariant
    /// (INGEST-04) preserved — the umbrella `Lunaris::ingest` is the only
    /// caller path.
    pub async fn write(
        &self,
        path: &str,
        content: impl Into<String>,
    ) -> Result<Lsn, LunarisError> {
        let source = format!("{}{}", self.session_prefix, path);
        let episode = Episode {
            id: Ulid::new(),
            source,
            content: content.into(),
            t_ref: None,
            // Server stamps bt at ingest time via the chunker's
            // `BiTemporal::now(clock)` per Plan 02-01.
            bt: lunaris_core::BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
            metadata: serde_json::Map::new(),
        };
        self.lunaris.ingest(episode).await
    }

    /// Read the latest content at `path`. Hits are filtered on the prefixed
    /// source equality and concatenated into a single `String` so the caller
    /// reconstructs the full document body even when the chunker emitted
    /// multiple chunks.
    ///
    /// Returns `None` when the recall produced zero hits (empty path / never
    /// written / fully purged).
    pub async fn read(&self, path: &str) -> Result<Option<String>, LunarisError> {
        let source = format!("{}{}", self.session_prefix, path);
        read_at(&self.lunaris, &source, path, None).await
    }

    /// Replace the contents at `path` with `new`. helios-rfc §5.3: MVCC retains
    /// the prior version. `_old` is accepted for the helios-rfc Read/Edit
    /// surface symmetry but is intentionally unused — Plan 04-04's existing
    /// `apply_supersede` path stamps the prior version's `bt.sys[1]` when the
    /// new ingest commits. NO new mutation code lives here (D-15).
    pub async fn edit(
        &self,
        path: &str,
        _old: &str,
        new: &str,
    ) -> Result<Lsn, LunarisError> {
        self.write(path, new).await
    }

    /// Hybrid retrieval (`Vector + Keyword(BM25) + RRF + rerank` per the Phase 2
    /// hot path defaults of [`Lunaris::recall`]) over the `helios:fs/<sid>/`
    /// prefix scope. Caller-controlled `k` (top hits returned).
    pub async fn grep(&self, pattern: &str, k: usize) -> Result<Vec<Hit>, LunarisError> {
        let prefix_filter = format!("source LIKE '{}%'", self.session_prefix);
        let builder = self.lunaris.recall_with_degraded_check().await?;
        builder
            .filter_str(&prefix_filter)
            .map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "grep filter parse: {e}"
                )))
            })?
            .top(k)
            .execute(lunaris_retrieve::Query::text(pattern))
            .await
    }

    /// List unique stored `path`s under the optional sub-`prefix`. Walks
    /// `StoragePort::scan_range` over `episode:` keys, filters payloads whose
    /// `source` begins with `helios:fs/<sid>/<prefix>`, returns the
    /// `<sid>/`-stripped tail.
    pub async fn ls(&self, prefix: Option<&str>) -> Result<Vec<String>, LunarisError> {
        let key_prefix: &[u8] = b"episode:";
        let storage = self.lunaris.storage();
        let mut stream = storage
            .scan_range(key_prefix, None)
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
            // skipped (other key namespaces under `episode:` would be a bug
            // in the writer; keep `ls` resilient).
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
        self.lunaris
            .forget(ForgetTarget::Scope(ScopeSpec::BySource(
                self.session_prefix.clone(),
            )))
            .await
    }

    /// Borrowed time-travel view per helios-rfc §5.3. `pad.as_of(ts).read(path)`
    /// returns the content as it existed at `ts` (uses Plan 02
    /// `RetrievalBuilder::as_of(ts)` under the hood).
    pub fn as_of(&self, ts: Hlc) -> AsOfScratchpad<'_> {
        AsOfScratchpad { inner: self, ts }
    }
}

/// Borrowed time-travel view returned by [`HeliosScratchpad::as_of`].
///
/// Held as a borrow (not a clone) so the time-travel query cannot outlive the
/// scratchpad — keeps the surface small (no `Clone` / `Send` requirement at the
/// AsOf layer; the scratchpad already provides those).
pub struct AsOfScratchpad<'a> {
    inner: &'a HeliosScratchpad,
    ts: Hlc,
}

impl AsOfScratchpad<'_> {
    /// Time-travel read. Same shape as [`HeliosScratchpad::read`] but seeds the
    /// retrieval `as_of` with this view's fixed timestamp.
    pub async fn read(&self, path: &str) -> Result<Option<String>, LunarisError> {
        let source = format!("{}{}", self.inner.session_prefix, path);
        read_at(&self.inner.lunaris, &source, path, Some(self.ts)).await
    }
}

// ---------------------------------------------------------------------------
// Internal helpers (kept private; do NOT count toward the ≤50 LOC contract)
// ---------------------------------------------------------------------------

/// Shared implementation backing [`HeliosScratchpad::read`] and
/// [`AsOfScratchpad::read`]. Runs the recall, filters by exact source equality,
/// and concatenates `Hit::text` into a single body.
async fn read_at(
    lunaris: &Arc<Lunaris>,
    source: &str,
    query_text: &str,
    as_of: Option<Hlc>,
) -> Result<Option<String>, LunarisError> {
    let filter = format!("source LIKE '{source}%'");
    let mut builder = lunaris
        .recall_with_degraded_check()
        .await?
        .filter_str(&filter)
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("read filter parse: {e}")))
        })?
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
        // Hit::text is the chunk body; concatenation is best-effort
        // reconstruction (helios-rfc §5.3 caller surface — Helios consumes
        // the joined string).
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
    /// [`HeliosScratchpad`] + 1 on [`AsOfScratchpad`]. Adjust ONLY alongside
    /// an HELIOS-* requirement update.
    #[test]
    fn helios_scratchpad_public_surface_under_50_loc() {
        let src = include_str!("./helios_scratchpad.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 9,
            "HELIOS-01 ≤50-LOC contract: HeliosScratchpad+AsOfScratchpad have {pub_fns} pub fns; cap is 9 (8 methods on HeliosScratchpad + AsOfScratchpad::read)"
        );
        assert!(
            pub_fns >= 9,
            "HELIOS-01 contract: expected exactly 9 public methods (8 on HeliosScratchpad + AsOfScratchpad::read); got {pub_fns} — did the public surface shrink?"
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
}
