//! Recency-ordered, source-filtered episode recall for the SessionStart digest.
//!
//! `RecallForPrompt` has no source filter and a semantic query biases to
//! similarity, not recency — neither surfaces "the scope's most-recent durable
//! decisions", which is exactly MEMORY.md's auto-load role. [`recent_by_source`]
//! walks ONLY the scope's `episode:` partition (scoped prefix via
//! [`keyspace::episode_prefix`]), keeps rows whose `source` starts with any
//! requested prefix, orders by `Episode.id` (ULID — time-then-random, so DESC
//! is recency-DESC), and returns the newest `limit`.
//!
//! Mirrors `forget::scan_matches_scoped` for the scan, but does NOT call
//! `read_as_of`: the scan value IS the episode, and recency comes from the ULID
//! key, so one pass over the partition suffices. Unparseable rows are skipped so
//! a single corrupt value never aborts the digest.

use futures::stream::StreamExt;

use lunaris_core::{Episode, LunarisError, Scope, StoragePort, keyspace};

/// Return the `limit` most-recent episodes under `scope` whose `source` starts
/// with any string in `prefixes`, newest first.
///
/// # Errors
/// Propagates the underlying [`StoragePort::scan_range`] error so the caller can
/// decide the failure policy (the contextd digest handler degrades to an empty
/// response — a digest failure must never block session start).
pub async fn recent_by_source(
    storage: &dyn StoragePort,
    scope: &Scope,
    prefixes: &[String],
    limit: usize,
) -> Result<Vec<Episode>, LunarisError> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    // Scope-partitioned scan prefix. Moon's `scan_range` matches the prefix
    // LITERALLY (it ignores its scope arg), so the scoped prefix is mandatory
    // for backend correctness — SQLite keys are `episode:{ulid}`, Moon keys are
    // `lunaris:{scope}:episode:{ulid}`; `episode_prefix` yields the right one.
    let prefix = keyspace::episode_prefix(scope);

    let mut stream =
        storage.scan_range(scope, &prefix, None).await.map_err(LunarisError::Storage)?;

    let mut matched: Vec<Episode> = Vec::new();
    while let Some(item) = stream.next().await {
        let (_key, value) = item.map_err(LunarisError::Storage)?;
        // A single corrupt/foreign value must not abort the whole digest.
        let episode: Episode = match serde_json::from_slice(&value) {
            Ok(ep) => ep,
            Err(_) => continue,
        };
        if prefixes.iter().any(|p| episode.source.starts_with(p.as_str())) {
            matched.push(episode);
        }
    }

    // ULID Ord is timestamp-then-randomness, so id DESC == recency DESC.
    matched.sort_by(|a, b| b.id.cmp(&a.id));
    matched.truncate(limit);
    Ok(matched)
}
