//! `scan_page` — typed, scope-bound, forward-paginated read over [`StoragePort::scan_range`].
//!
//! ADD task `read-api-pagination`, contract v1 (FROZEN 2026-06-16). The shared read-API
//! envelope every Memory-Inspector browse endpoint binds to. It turns `scan_range`'s
//! UNORDERED, possibly mid-stream-failing byte stream into a deterministic [`Page<T>`] of
//! deserialized primitives with an opaque, forward-only cursor.
//!
//! The helper IMPOSES ULID ordering (it does not trust the backend's scan order — Moon's
//! `SCAN MATCH` is unordered and already buffers the whole prefix, so the sort adds no new
//! asymptotic cost there) and validates inputs BEFORE any I/O, so a rejected call never
//! touches the backend. A mid-stream scan error surfaces as [`ListError::Storage`] rather
//! than a short page passed off as complete.

use bytes::Bytes;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use ulid::Ulid;

use crate::error::StorageError;
use crate::hlc::Hlc;
use crate::scope::Scope;

use super::StoragePort;

/// Hard upper bound on a single page (contract v1). A `limit` above this is rejected.
pub const MAX_PAGE: usize = 500;

/// One page of [`scan_page`] results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    /// Up to `limit` items, ascending by ULID key.
    pub items: Vec<T>,
    /// Opaque forward cursor: `Some` iff more items remain under the prefix, `None` on the
    /// last page. Pass it back verbatim to fetch the next page — callers never parse it.
    pub next_cursor: Option<String>,
}

/// Failure modes of [`scan_page`]. [`ListError::code`] is the stable wire string.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    /// `limit == 0`.
    #[error("invalid_limit: limit must be >= 1")]
    InvalidLimit,
    /// `limit > MAX_PAGE`.
    #[error("limit_too_large: limit exceeds the maximum page size (500)")]
    LimitTooLarge,
    /// The supplied cursor is not a valid ULID token.
    #[error("invalid_cursor: cursor is not a valid ULID token")]
    InvalidCursor,
    /// A value on the page failed to deserialize into `T`.
    #[error("corrupt_row: a page value failed to deserialize")]
    CorruptRow,
    /// A backend / mid-stream scan error, propagated.
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
}

impl ListError {
    /// Stable, machine-readable code — the wire string the HTTP layer surfaces.
    pub fn code(&self) -> &'static str {
        match self {
            ListError::InvalidLimit => "invalid_limit",
            ListError::LimitTooLarge => "limit_too_large",
            ListError::InvalidCursor => "invalid_cursor",
            ListError::CorruptRow => "corrupt_row",
            ListError::Storage(_) => "storage",
        }
    }
}

/// Read one page of primitives of type `T` under `prefix`, scope-bound, ULID-ascending.
///
/// `prefix` is a `lunaris_core::keyspace::*_prefix(scope)` value; `cursor` is `None`/`""`
/// for the first page or a token returned by a prior call; `limit` must be `1..=MAX_PAGE`;
/// `as_of` is passed through to [`StoragePort::scan_range`] unchanged (Phase-1 callers pass
/// `None` = current state). See the `read-api-pagination` contract for the full rule set.
pub async fn scan_page<T: DeserializeOwned>(
    port: &dyn StoragePort,
    scope: &Scope,
    prefix: &[u8],
    cursor: Option<&str>,
    limit: usize,
    as_of: Option<Hlc>,
) -> Result<Page<T>, ListError> {
    // 1. Validate inputs BEFORE any I/O so a rejected call never scans the backend.
    if limit == 0 {
        return Err(ListError::InvalidLimit);
    }
    if limit > MAX_PAGE {
        return Err(ListError::LimitTooLarge);
    }
    // A present, non-empty cursor must be a valid ULID; build its exclusive lower-bound key.
    let after_key: Option<Vec<u8>> = match cursor {
        None | Some("") => None,
        Some(c) => {
            let ulid = Ulid::from_string(c).map_err(|_| ListError::InvalidCursor)?;
            let mut k = prefix.to_vec();
            k.extend_from_slice(ulid.to_string().as_bytes());
            Some(k)
        }
    };

    // 2. Drain scan_range; the FIRST per-row Err propagates (never a short page sold whole).
    let mut stream = port.scan_range(scope, prefix, as_of).await?;
    let mut pairs: Vec<(Vec<u8>, Bytes)> = Vec::new();
    while let Some(item) = stream.next().await {
        let (key, value) = item?;
        pairs.push((key.to_vec(), value));
    }

    // 3. Impose ascending key order — scan_range does not guarantee it (e.g. Moon SCAN).
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    // 4. Window: keys strictly greater than the cursor key, then take `limit`.
    let start = match &after_key {
        None => 0,
        Some(k) => pairs.partition_point(|(key, _)| key.as_slice() <= k.as_slice()),
    };
    let remaining = &pairs[start..];
    let take = remaining.len().min(limit);
    let window = &remaining[..take];

    // 5. Deserialize ONLY the window; any failure stops the page (transparency > availability).
    let mut items = Vec::with_capacity(window.len());
    for (_key, value) in window {
        let item: T = serde_json::from_slice(value.as_ref()).map_err(|_| ListError::CorruptRow)?;
        items.push(item);
    }

    // 6. next_cursor = ULID of the window's last key, iff ≥1 key remains after the window.
    let next_cursor = if remaining.len() > take {
        let last_key = &window[window.len() - 1].0;
        Some(ulid_from_key(last_key, prefix).ok_or(ListError::CorruptRow)?.to_string())
    } else {
        None
    };

    Ok(Page { items, next_cursor })
}

/// Extract the trailing ULID from a `prefix ++ {ulid}` key. `None` if the key does not
/// carry the prefix or the suffix is not a valid ULID.
fn ulid_from_key(key: &[u8], prefix: &[u8]) -> Option<Ulid> {
    let suffix = key.strip_prefix(prefix)?;
    let s = std::str::from_utf8(suffix).ok()?;
    Ulid::from_string(s).ok()
}
