//! Hit hydration — RETRIEVE-10.
//!
//! For each `RawHit`, look up the chunk JSON via `StoragePort::read_as_of`,
//! deserialize, and project the chunk's text + heading_path + bi-temporal
//! stamp into a `Hit`. Episode `source` lookup is batched: collect the unique
//! `episode_id`s first, fetch episodes once, then fan back out.
//!
//! ## Failure modes
//!
//! - `read_as_of` returns `Err` → propagates as `LunarisError::Storage(_)`.
//! - `read_as_of` returns `Ok(None)` (chunk since-deleted at hydration time)
//!   → the hit is SKIPPED (never returned). The plan-level test
//!   `hydrate_returns_chunk_text_and_source` asserts the happy path.
//! - JSON deserialize errors → propagate as `LunarisError::Storage(Serde(_))`.
//!
//! Note: `as_of = None` falls back to `HlcClock::now()` for the lookup so we
//! always read the live row.

use std::collections::HashMap;

use lunaris_core::{Chunk, Episode, Hlc, HlcClock, LunarisError, Scope, StoragePort};
use ulid::Ulid;

use crate::types::{Hit, RawHit};

/// Build the chunk lookup key from the raw hit's id bytes.
///
/// The Phase 2 ingest pipeline writes chunks at key `lunaris:chunk:<ulid>`
/// and stores the same ulid bytes (16) in `VectorUpsert.id`. So we recover
/// the ulid string from the bytes and prepend the prefix.
fn chunk_lookup_key(id_bytes: &[u8]) -> Option<Vec<u8>> {
    let ulid = Ulid::from_bytes(id_bytes.try_into().ok()?);
    Some(format!("lunaris:chunk:{ulid}").into_bytes())
}

/// Build the episode lookup key from a ulid.
fn episode_lookup_key(id: Ulid) -> Vec<u8> {
    format!("lunaris:episode:{id}").into_bytes()
}

/// Hydrate a list of `RawHit`s into full `Hit`s.
///
/// Looks up each chunk and its parent episode (batched per unique episode_id).
/// Hits whose chunk row is missing are skipped (since-deleted chunks).
///
/// `initial_degraded` (Plan 04-04 B-9) is OR-ed into every produced
/// `Hit::degraded` so callers using `Lunaris::recall_with_degraded_check`
/// see the verifier-queue-lag flag on every hit.
pub async fn hydrate(
    storage: &dyn StoragePort,
    hits: Vec<RawHit>,
    as_of: Option<Hlc>,
    initial_degraded: bool,
) -> Result<Vec<Hit>, LunarisError> {
    // The fallback "live" timestamp when as_of is None.
    let live_clock = HlcClock::new(0);
    let snapshot = as_of.unwrap_or_else(|| live_clock.tick());

    // First pass: pull chunk rows.
    let mut chunks: Vec<(RawHit, Chunk)> = Vec::with_capacity(hits.len());
    for raw in hits {
        let key = match chunk_lookup_key(&raw.id) {
            Some(k) => k,
            None => continue, // bytes don't decode to a ulid — skip
        };
        // RFC 0001 Wave 1D: hydrate reads under Scope::dev() until the
        // RetrievalBuilder carries per-scope routing (Wave 1C backend plumbing).
        let dev_scope = Scope::dev();
        match storage.read_as_of(&dev_scope, &key, snapshot).await? {
            Some(row) => {
                let chunk: Chunk = match serde_json::from_slice(&row.value) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                chunks.push((raw, chunk));
            }
            None => {
                // Chunk no longer exists at this snapshot — skip
                continue;
            }
        }
    }

    // Second pass: batch episode lookups by unique episode_id.
    let unique_ep: Vec<Ulid> = {
        let mut s = std::collections::HashSet::new();
        for (_, c) in &chunks {
            s.insert(c.episode_id);
        }
        s.into_iter().collect()
    };

    let mut episode_sources: HashMap<Ulid, String> = HashMap::new();
    let dev_scope = Scope::dev();
    for ep_id in unique_ep {
        let key = episode_lookup_key(ep_id);
        if let Some(row) = storage.read_as_of(&dev_scope, &key, snapshot).await?
            && let Ok(ep) = serde_json::from_slice::<Episode>(&row.value)
        {
            episode_sources.insert(ep_id, ep.source);
        }
    }

    // Third pass: project to Hits.
    Ok(chunks
        .into_iter()
        .map(|(raw, chunk)| Hit {
            id: raw.id,
            score: raw.score,
            text: chunk.text,
            source: episode_sources.get(&chunk.episode_id).cloned().unwrap_or_default(),
            heading_path: chunk.heading_path,
            valid_from: chunk.bt.valid.0,
            valid_to: chunk.bt.valid.1,
            // Plan 02-03: degraded + rerank_applied flow through hydration so
            // callers can tell whether the hit came from a degraded_fallback
            // path (`degraded == true`) and whether the rerank pass actually
            // ran (`rerank_applied == true` only when a non-NoopReranker fired).
            //
            // Plan 04-04 B-9: OR `initial_degraded` so hits also surface the
            // verifier-queue-lag backpressure flag from
            // `Lunaris::recall_with_degraded_check`.
            degraded: raw.degraded || initial_degraded,
            rerank_applied: raw.rerank_applied,
            source_op: raw.source_op,
        })
        .collect())
}

/// Partial hydration — fetch ONLY chunk text for the given raw hits, returning
/// a `HashMap<id_bytes -> chunk.text>`.
///
/// Used by Plan 02-03's `rerank` operator: the cross-encoder needs the chunk
/// body to pair-encode `(query, doc.text)` for scoring, but doesn't need the
/// parent Episode's `source` or any other Hit-level field. Skipping the
/// episode lookup roundtrip is the v0 latency-budget guard so the rerank pass
/// stays inside the blueprint §4.2 12 ms allocation.
///
/// Hits whose chunk row is missing at the snapshot are SKIPPED (no entry in
/// the returned map). Callers fall back to `text = ""` for missing entries
/// — the cross-encoder will produce a low score and the downstream `top(n)`
/// truncates them out.
pub async fn partial_hydrate_text(
    storage: &dyn StoragePort,
    hits: &[RawHit],
    as_of: Option<Hlc>,
) -> Result<HashMap<Vec<u8>, String>, LunarisError> {
    let live_clock = HlcClock::new(0);
    let snapshot = as_of.unwrap_or_else(|| live_clock.tick());

    let dev_scope = Scope::dev();
    let mut by_id: HashMap<Vec<u8>, String> = HashMap::with_capacity(hits.len());
    for raw in hits {
        let key = match chunk_lookup_key(&raw.id) {
            Some(k) => k,
            None => continue,
        };
        match storage.read_as_of(&dev_scope, &key, snapshot).await? {
            Some(row) => {
                if let Ok(chunk) = serde_json::from_slice::<Chunk>(&row.value) {
                    by_id.insert(raw.id.clone(), chunk.text);
                }
            }
            None => {
                // Chunk no longer exists at this snapshot — skip
                continue;
            }
        }
    }
    Ok(by_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_lookup_key_round_trips_ulid() {
        let id = Ulid::new();
        let bytes = id.to_bytes().to_vec();
        let key = chunk_lookup_key(&bytes).unwrap();
        let s = String::from_utf8(key).unwrap();
        assert!(s.starts_with("lunaris:chunk:"));
        assert!(s.contains(&id.to_string()));
    }

    #[test]
    fn chunk_lookup_key_rejects_wrong_size() {
        assert!(chunk_lookup_key(b"too-short").is_none());
    }
}
