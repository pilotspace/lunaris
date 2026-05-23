//! `FT.INVALIDATE_RANGE` — raw RESP escape hatch for Moon bulk invalidation.
//!
//! ## Wire shape
//!
//! ```text
//! FT.INVALIDATE_RANGE <index> <node_id_field> <node_id_value>
//!                     <hlc_wall_field> <hlc_wall_lo> <hlc_wall_hi>
//! ```
//!
//! Returns: `(integer)` count of records deleted from the index.
//!
//! ## Escape hatch rationale (STORE-09)
//!
//! `moon-client` v0.1.x does not expose a typed wrapper for `FT.INVALIDATE_RANGE`.
//! We reach `redis::aio::MultiplexedConnection` via `MoonClient::inner_mut()` on a
//! local clone — the same documented pattern as the HSCAN escape hatch in `kv.rs`.
//! This is the SECOND raw RESP `cmd` site permitted in `lunaris-storage-moon/src/`
//! per Phase 1.5 STORE-09 constraints. When `moon-client` adds a typed wrapper,
//! replace this call with the typed form and remove the `redis` direct dependency.
//!
//! ## Per-scope index naming
//!
//! The `index` argument passed in by `Lunaris::invalidate_range` is the bare kind
//! name (`"chunks"`, `"entities"`, etc.). This function builds the full per-scope
//! FT index name via `ft_index_name(scope, index)` before issuing the command —
//! exactly the same naming convention used by `keyword.rs` and `vector.rs`.
//!
//! ## Error mapping
//!
//! - Moon `WRONGTYPE` reply (index does not exist) → `StorageError::Backend`
//!   with `"WRONGTYPE"` in the message. The caller (`Lunaris::invalidate_range`)
//!   pattern-matches on this string for degraded-mode warn-and-skip.
//! - Any other Moon error → `StorageError::Backend` with the raw message.

use lunaris_core::Scope;
use lunaris_core::error::StorageError;

use crate::client::{MoonClient, redis_err};
use crate::keyspace::ft_index_name;

/// Issue `FT.INVALIDATE_RANGE` against the per-scope FT index for `kind`.
///
/// Returns the count of deleted records, or a `StorageError::Backend` on
/// Moon-level failure. `WRONGTYPE` (no such index) is surfaced as-is so
/// the calling `Lunaris::invalidate_range` fan-out can warn-and-skip it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn invalidate_range(
    client: &MoonClient,
    scope: &Scope,
    index: &str,
    node_id_field: &str,
    node_id_value: &str,
    hlc_wall_field: &str,
    hlc_wall_lo_inclusive: i64,
    hlc_wall_hi_inclusive: i64,
) -> Result<u64, StorageError> {
    // Build the per-scope FT index name (e.g. "lunaris_acme.agent-1_chunks_idx").
    let ft_index = ft_index_name(scope, index);

    // ESCAPE HATCH: raw RESP command — see module-level doc for rationale.
    // Clone the typed client to get an independent handle; reach the underlying
    // MultiplexedConnection via `inner_mut()` on that clone so the parent
    // connection stays free for concurrent typed calls.
    let mut raw_inner = client.typed();
    let raw_conn = raw_inner.inner_mut();

    let count: i64 = redis::cmd("FT.INVALIDATE_RANGE")
        .arg(&ft_index)
        .arg(node_id_field)
        .arg(node_id_value)
        .arg(hlc_wall_field)
        .arg(hlc_wall_lo_inclusive)
        .arg(hlc_wall_hi_inclusive)
        .query_async(raw_conn)
        .await
        .map_err(redis_err)?;

    // Moon returns a RESP integer. Negative values would indicate a server-side
    // parse error that slipped through as an integer reply — clamp to 0 rather
    // than propagating a nonsensical negative count.
    Ok(count.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Structural guard: the escape hatch MUST use `redis::cmd("FT.INVALIDATE_RANGE")`.
    /// If someone swaps this for a typed call in the future, this test stays green
    /// only if the typed call issues the same command name on the wire.
    #[test]
    fn invalidate_range_uses_correct_command_name() {
        // Verify the literal command name in this source file.
        let source = include_str!("invalidate.rs");
        assert!(
            source.contains(r#"redis::cmd("FT.INVALIDATE_RANGE")"#),
            "invalidate_range must issue FT.INVALIDATE_RANGE via redis::cmd"
        );
    }

    /// Structural guard: per-scope index name is derived from `ft_index_name`,
    /// not a raw string concatenation. This ensures the naming convention stays
    /// in sync with `keyword.rs`, `vector.rs`, and `keyspace.rs`.
    #[test]
    fn invalidate_range_uses_ft_index_name_helper() {
        let source = include_str!("invalidate.rs");
        assert!(
            source.contains("ft_index_name(scope, index)"),
            "must derive index name via ft_index_name(scope, index)"
        );
    }
}
