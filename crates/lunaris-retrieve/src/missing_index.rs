//! F1 — a scope with no writes yet must recall EMPTY, not error.
//!
//! Moon creates a scope's FT index lazily, on first write. A scope nothing has
//! ever been ingested into therefore has no index, and `FT.SEARCH` answers
//! `unknown index`. Every recall leg propagated that with `?`, so the very
//! first thing a new agent does — ask what it remembers, before it has
//! remembered anything — was an error rather than an empty list.
//!
//! That is the worst possible place for a bug: it cannot be reached by an
//! existing deployment, only by a new one, so it survives any amount of
//! production traffic. It surfaced from the Python SDK's
//! `test_cross_scope_isolation`, which recalls under a scope it deliberately
//! never writes to (ledger F1), and three separate call sites had already
//! grown their own string-match workaround for it —
//! `primitives::working_memory`, `lunaris-hook::context`, and the LongMemEval
//! harness. Three private copies of one rule is the smell that says the rule
//! belongs somewhere shared.
//!
//! ## Why here and not in `lunaris-storage-moon`
//!
//! At the storage boundary the two cases are genuinely indistinguishable: a
//! never-written scope's `lunaris_{scope}_chunks_idx` and a typo'd
//! `lunaris_{scope}_unknown_table_idx` produce the identical Moon reply.
//! Swallowing it down there would turn a misconfigured index name into silent
//! empty results — the failure mode this codebase keeps having to dig out —
//! and `keyword_search_rejects_unknown_index_on_moon` pins that contract
//! deliberately.
//!
//! Up here the index name came from the plan, not from caller input, so
//! "absent" can only mean "nothing was written under this scope yet". That is
//! a fact about the data, and the honest answer to a search over no rows is no
//! rows.

use lunaris_core::error::StorageError;

/// `true` when the backend is telling us the scope's index does not exist yet.
///
/// String-matched because Moon reports it as a generic `ResponseError` with no
/// typed variant to match on. THREE spellings are checked, all observed in this
/// repo: `unknown index` (the hybrid and keyword paths),  `Unknown index`, and
/// `moon: redis error: "Unknown": Index name` (what `operators::tree` had been
/// matching, via `Index name`). Matching one spelling leaves the bug half-fixed
/// in a way no test against a single Moon build can catch — which is exactly
/// how four call sites ended up with three different predicates.
pub fn is_index_absent(err: &StorageError) -> bool {
    match err {
        StorageError::Backend(msg) => {
            msg.contains("unknown index")
                || msg.contains("Unknown index")
                || msg.contains("Index name")
        }
        _ => false,
    }
}

/// Map "this scope has no index yet" to "this scope has no rows".
///
/// Every other error passes through untouched — a backend that is down, a
/// malformed query, or a timeout must still surface. Wrap the storage call,
/// then `?` as before.
pub fn no_rows_if_index_absent<T>(r: Result<Vec<T>, StorageError>) -> Result<Vec<T>, StorageError> {
    match r {
        Err(e) if is_index_absent(&e) => Ok(Vec::new()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_index_becomes_no_rows() {
        let r: Result<Vec<u8>, StorageError> =
            Err(StorageError::Backend("redis error: ResponseError: unknown index".into()));
        assert_eq!(no_rows_if_index_absent(r).expect("must not error"), Vec::<u8>::new());
    }

    /// All three observed spellings, so a fix that lands for one path cannot
    /// silently miss another. The third is the one `operators::tree` matched
    /// on before this module existed.
    #[test]
    fn every_observed_spelling_is_recognised() {
        for msg in [
            "redis error: ResponseError: unknown index",
            "Unknown index: foo",
            "moon: redis error: \"Unknown\": Index name",
        ] {
            assert!(
                is_index_absent(&StorageError::Backend(msg.into())),
                "{msg:?} is Moon saying the index is absent, and was not recognised"
            );
        }
    }

    /// The half that makes this safe. A helper that swallowed everything would
    /// pass the test above and turn every outage into an empty recall.
    #[test]
    fn every_other_backend_error_still_surfaces() {
        for msg in [
            "connection refused",
            "MOONERR diskfull: writes paused until free space recovers",
            "timed out after 5s",
            "CrossSlot: Keys in MULTI/EXEC don't hash to the same shard",
        ] {
            let r: Result<Vec<u8>, StorageError> = Err(StorageError::Backend(msg.into()));
            assert!(
                no_rows_if_index_absent(r).is_err(),
                "{msg:?} was swallowed into an empty result — only a missing index may be"
            );
        }
    }

    /// Non-`Backend` variants are never index-absence, whatever they say.
    #[test]
    fn a_non_backend_variant_is_never_index_absence() {
        assert!(!is_index_absent(&StorageError::NotSupported("unknown index")));
    }
}
