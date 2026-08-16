//! `list_scopes` — enumerate known scopes via SCAN of the `lunaris:*` keyspace.
//!
//! Q-U2 lock 2026-05-17: lazy SCAN-derived enumeration. There is NO explicit
//! `set:scopes` index — every call full-scans `lunaris:*` and parses the
//! scope segment via [`parse_scope_from_key`]. This is O(N keys) per call
//! but acceptable for v1; the cost lives in the Moon SCAN cursor walk
//! (Moon's COUNT defaults batch size) and the caller pays via wall time, not
//! correctness. Promotion to an eager `SADD` index is deferred until SCAN
//! proves too slow under measured load.
//!
//! ## Cursor model
//!
//! Cursor is the last scope string emitted on the previous page (verbatim
//! UTF-8, opaque to the caller per Q-U1 lock). Resume = first scope strictly
//! greater than the cursor. `None` cursor = restart from the beginning.
//!
//! Returning `Some(last_emitted)` on a page that yields exactly `limit`
//! scopes signals "more available". Returning `None` signals exhausted.
//! Passing the largest emitted scope back as cursor returns an empty page —
//! the past-the-end safety check protecting retry paths.
//!
//! ## Why not stream Moon's SCAN cursor through?
//!
//! Two reasons:
//!
//! 1. **Dedupe across batches** — a scope with 100 episode keys, 50 chunk
//!    keys, 20 entity keys spans many SCAN batches; we must dedupe across
//!    them. Carrying just Moon's cursor leaks duplicates back to the caller.
//! 2. **Stable pagination under writes** — Moon SCAN guarantees a key
//!    present for the whole iteration is returned exactly once, but does
//!    NOT guarantee anything about keys written/deleted during iteration.
//!    A scope-string cursor decouples our pagination from those races —
//!    new scopes written between page 1 and page 2 simply appear at their
//!    sorted position on the next call.
//!
//! The cost: every page walks the full keyspace. Documented + measurable.

use std::collections::BTreeSet;

use lunaris_core::error::StorageError;
use lunaris_core::keyspace::parse_scope_from_key;
use lunaris_core::scope::Scope;
use lunaris_core::storage::types::ScopePage;

use crate::client::{MoonClient, redis_err};

/// Per-batch COUNT hint for `SCAN`. Moon's SCAN treats COUNT as a hint, not a
/// limit — the actual batch size may vary. 1000 matches `kv::scan_range`.
const SCAN_BATCH_COUNT: usize = 1000;

/// Implementation of `StoragePort::list_scopes` for the Moon backend.
///
/// Walks `SCAN MATCH lunaris:*` to completion, parses each key via
/// [`parse_scope_from_key`], dedupes into a `BTreeSet`, applies the prefix
/// filter and cursor, then returns at most `limit` scopes plus a continuation
/// cursor.
///
/// `limit == 0` short-circuits to an empty page with no cursor (no Moon
/// round-trip).
pub(crate) async fn list_scopes(
    c: &MoonClient,
    prefix: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
) -> Result<ScopePage, StorageError> {
    if limit == 0 {
        return Ok(ScopePage::default());
    }

    // Bound MATCH to `lunaris:{prefix-fragment}*` when a prefix is supplied,
    // otherwise `lunaris:*`. Moon's MATCH is glob (not regex) — a literal
    // prefix is safe because Scope::new's regex rejects every glob meta-
    // character (`*`, `?`, `[`, `]`, `\`). Documented in the doc-comment.
    let pattern = match prefix {
        Some(p) => format!("lunaris:{p}*"),
        None => "lunaris:*".to_string(),
    };

    // SCAN to completion. We need a per-connection mutable handle for the raw
    // RESP `SCAN` command (moon-client v0.1.x doesn't expose a typed scope
    // enumerator — same escape hatch as `kv::scan_range`).
    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();

    let mut all: BTreeSet<String> = BTreeSet::new();
    let mut moon_cursor: u64 = 0;
    loop {
        let (next, batch): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN")
            .arg(moon_cursor)
            .arg("MATCH")
            .arg(pattern.as_str())
            .arg("COUNT")
            .arg(SCAN_BATCH_COUNT)
            .query_async(raw_conn)
            .await
            .map_err(redis_err)?;
        for k in batch {
            if let Some(scope) = parse_scope_from_key(&k) {
                all.insert(scope.as_str().to_string());
            }
            // Non-Lunaris keys (or keys whose scope segment fails Scope::new's
            // regex) are silently skipped — `list_scopes` reports the scopes
            // Lunaris owns, never every key in the Moon instance.
        }
        if next == 0 {
            break;
        }
        moon_cursor = next;
    }

    // Apply prefix filter (Moon's MATCH is best-effort; re-apply in Rust to
    // catch any glob-misinterpretation edge cases AND to enforce strict
    // byte-prefix semantics).
    let mut filtered: Vec<String> =
        all.into_iter().filter(|s| prefix.is_none_or(|p| s.starts_with(p))).collect();

    // Apply cursor: skip everything <= cursor (resume strictly greater).
    if let Some(after) = cursor {
        match filtered.iter().position(|s| s.as_str() > after) {
            Some(idx) => {
                filtered.drain(..idx);
            }
            None => {
                // Cursor at-or-past largest — past-the-end empty page.
                filtered.clear();
            }
        }
    }

    let (page, next_cursor) = if filtered.len() > limit {
        let page: Vec<_> = filtered.drain(..limit).collect();
        let last = page.last().cloned();
        (page, last)
    } else {
        (filtered, None)
    };

    // Re-construct Scope values. `parse_scope_from_key` already validated via
    // `Scope::new`; the round-trip cannot fail. `.expect("invariant: …")`
    // documents the by-construction guarantee per CLAUDE.md no-unwrap rule.
    let scopes: Vec<Scope> = page
        .into_iter()
        .map(|s| {
            Scope::new(&s).expect("invariant: parse_scope_from_key yields Scope-valid strings")
        })
        .collect();

    Ok(ScopePage { scopes, next_cursor })
}
