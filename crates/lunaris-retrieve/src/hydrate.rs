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
//!
//! ## Plan 260610-f91: concurrent fan-out
//!
//! All serial `for raw in hits { read_as_of(...).await? }` loops are replaced
//! with `futures::stream::iter(...).buffered(HYDRATE_CONCURRENCY)` (chunk pass
//! and partial_hydrate_text, order-preserving) and `buffer_unordered` (episode
//! pass, order irrelevant — it builds a HashMap). HYDRATE_CONCURRENCY=32.

use std::collections::HashMap;

use futures::stream::{self, StreamExt};
use lunaris_core::keyspace::{chunk_key, episode_key, fact_key};
use lunaris_core::{BiTemporal, Chunk, Episode, Hlc, HlcClock, LunarisError, Scope, StoragePort};
use ulid::Ulid;

use crate::types::{Hit, RawHit};

const HYDRATE_CONCURRENCY: usize = 32;

/// Build the chunk lookup key from the raw hit's id bytes.
///
/// RFC 0001 Wave 1C: chunk keys are scope-prefixed via
/// `lunaris_core::keyspace::chunk_key` (Wave 2.5B: moved from lunaris-storage-moon).
/// The reader MUST use the same helper so writer/reader key formats stay in lockstep.
fn chunk_lookup_key(scope: &Scope, id_bytes: &[u8]) -> Option<Vec<u8>> {
    let ulid = Ulid::from_bytes(id_bytes.try_into().ok()?);
    Some(chunk_key(scope, ulid))
}

/// Build the episode lookup key from a ulid (scope-prefixed per RFC 0001 Wave 1C).
fn episode_lookup_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    episode_key(scope, id)
}

/// Sys-closed = logically deleted for current reads (ADD task
/// forget-scope-routing). `ScopedLunaris::forget` soft-deletes by stamping
/// `bt.sys.1`; supersede/invalidate stamp the same tuple — either way the row
/// is no longer current and MUST NOT hydrate.
fn sys_closed(bt: &BiTemporal) -> bool {
    bt.sys.1.is_some()
}

/// Hydrate a list of `RawHit`s into full `Hit`s.
///
/// Looks up each chunk and its parent episode (batched per unique episode_id).
/// Hits whose chunk row is missing are skipped (since-deleted chunks).
///
/// `scope` narrows the KV lookups to the correct scope partition (Wave 2.5C).
/// `initial_degraded` (Plan 04-04 B-9) is OR-ed into every produced
/// `Hit::degraded` so callers using `Lunaris::recall_with_degraded_check`
/// see the verifier-queue-lag flag on every hit.
pub async fn hydrate(
    storage: &dyn StoragePort,
    scope: &Scope,
    hits: Vec<RawHit>,
    as_of: Option<Hlc>,
    initial_degraded: bool,
) -> Result<Vec<Hit>, LunarisError> {
    // The fallback "live" timestamp when as_of is None.
    let live_clock = HlcClock::new(0);
    let snapshot = as_of.unwrap_or_else(|| live_clock.tick());

    // First pass: pull chunk rows concurrently using the caller-supplied scope.
    // Plan 260610-f91: replace serial for loop with buffered(HYDRATE_CONCURRENCY)
    // to fan out all read_as_of calls in parallel (order-preserving).
    let chunks: Vec<(RawHit, Chunk)> = {
        let results: Vec<Result<Option<(RawHit, Chunk)>, LunarisError>> = stream::iter(hits)
            .map(|raw| {
                let scope = scope.clone();
                async move {
                    let key = match chunk_lookup_key(&scope, &raw.id) {
                        Some(k) => k,
                        None => return Ok(None), // bytes don't decode to a ulid — skip
                    };
                    match storage.read_as_of(&scope, &key, snapshot).await? {
                        Some(row) => {
                            let chunk: Chunk = match serde_json::from_slice(&row.value) {
                                Ok(c) => c,
                                Err(_) => return Ok(None),
                            };
                            if sys_closed(&chunk.bt) {
                                return Ok(None); // soft-deleted chunk row — skip
                            }
                            Ok(Some((raw, chunk)))
                        }
                        None => Ok(None), // chunk no longer exists at this snapshot — skip
                    }
                }
            })
            .buffered(HYDRATE_CONCURRENCY)
            .collect()
            .await;
        results.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().flatten().collect()
    };

    // Second pass: batch episode lookups by unique episode_id.
    let unique_ep: Vec<Ulid> = {
        let mut s = std::collections::HashSet::new();
        for (_, c) in &chunks {
            s.insert(c.episode_id);
        }
        s.into_iter().collect()
    };

    // Episode pass: order irrelevant (builds a HashMap), use buffer_unordered.
    // Plan 260610-f91: concurrent fan-out for episode lookups. Storage errors
    // PROPAGATE (matching the pre-fan-out `.await?` contract); only missing
    // rows and deserialize failures are skipped.
    // Map ep_id -> (source, sys_closed). A sys-closed parent episode means the
    // episode was forgotten/invalidated — its chunks MUST NOT hydrate even
    // though the chunk rows themselves stay sys-open (forget v0 stamps only
    // the `episode:` row).
    let mut episode_sources: HashMap<Ulid, (String, bool)> = HashMap::new();
    let ep_results: Vec<Result<Option<(Ulid, String, bool)>, LunarisError>> =
        stream::iter(unique_ep)
            .map(|ep_id| {
                let scope = scope.clone();
                async move {
                    let key = episode_lookup_key(&scope, ep_id);
                    if let Some(row) = storage.read_as_of(&scope, &key, snapshot).await?
                        && let Ok(ep) = serde_json::from_slice::<Episode>(&row.value)
                    {
                        return Ok(Some((ep_id, ep.source, sys_closed(&ep.bt))));
                    }
                    Ok(None)
                }
            })
            .buffer_unordered(HYDRATE_CONCURRENCY)
            .collect()
            .await;
    for entry in ep_results.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().flatten() {
        episode_sources.insert(entry.0, (entry.1, entry.2));
    }

    // Third pass: project to Hits, dropping chunks whose parent episode is
    // sys-closed (forgotten). Missing episodes stay tolerated (empty source)
    // — pre-existing behaviour for orphaned chunks.
    Ok(chunks
        .into_iter()
        .filter(|(_, chunk)| !matches!(episode_sources.get(&chunk.episode_id), Some((_, true))))
        .map(|(raw, chunk)| Hit {
            id: raw.id,
            // In-process provenance: lets exact-key callers (e.g.
            // WorkingMemory::read) recover the verbatim Episode `content`
            // rather than the lossy chunk `text`. `#[serde(skip)]` keeps it
            // off the wire — see `Hit::episode_id`.
            episode_id: chunk.episode_id.to_bytes().to_vec(),
            score: raw.score,
            text: chunk.text,
            source: episode_sources
                .get(&chunk.episode_id)
                .map(|(s, _)| s.clone())
                .unwrap_or_default(),
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

/// Per-hit resolution for [`hydrate_mixed`] — which at-rest row the id bytes
/// resolved to. Facts carry the KV row's bi-temporal stamp (the at-rest
/// `lunaris_extract::Fact` keeps validity as ISO strings, not `Hlc`).
enum MixedResolved {
    Chunk(RawHit, Chunk),
    Fact(RawHit, lunaris_extract::Fact, BiTemporal),
}

/// Hydrate fused hybrid hits that may mix CHUNK ids and FACT ids
/// (hook-recall-graph-hybrid, contract v1.1).
///
/// Resolution per hit, order-preserving:
/// 1. chunk row at `keyspace::chunk_key` → byte-identical to [`hydrate`]
///    semantics (chunk text + batched episode-source lookup + heading_path +
///    the chunk's bi-temporal stamp);
/// 2. else fact row at `keyspace::fact_key` → the at-rest
///    `lunaris_extract::Fact` (heterogeneous read model): `text = fact_text`,
///    `source = "fact:{predicate}"`, stamp from the KV row;
/// 3. else the hit is DROPPED (mirrors the since-deleted-chunk skip).
///
/// `initial_degraded` ORs into every produced hit exactly as in [`hydrate`]
/// (Plan 04-04 B-9 parity — the fact path must not hide backpressure flags).
pub async fn hydrate_mixed(
    storage: &dyn StoragePort,
    scope: &Scope,
    hits: Vec<RawHit>,
    as_of: Option<Hlc>,
    initial_degraded: bool,
) -> Result<Vec<Hit>, LunarisError> {
    let live_clock = HlcClock::new(0);
    let snapshot = as_of.unwrap_or_else(|| live_clock.tick());

    // First pass: resolve each id to a chunk row, else a fact row, else drop.
    // Concurrent buffered fan-out, order-preserving (Plan 260610-f91 pattern).
    let resolved: Vec<MixedResolved> = {
        let results: Vec<Result<Option<MixedResolved>, LunarisError>> = stream::iter(hits)
            .map(|raw| {
                let scope = scope.clone();
                async move {
                    let ulid = match <[u8; 16]>::try_from(raw.id.as_slice()) {
                        Ok(bytes) => Ulid::from_bytes(bytes),
                        Err(_) => return Ok(None), // bytes don't decode — skip
                    };
                    if let Some(row) =
                        storage.read_as_of(&scope, &chunk_key(&scope, ulid), snapshot).await?
                        && let Ok(chunk) = serde_json::from_slice::<Chunk>(&row.value)
                    {
                        if sys_closed(&chunk.bt) {
                            return Ok(None); // soft-deleted chunk row — skip
                        }
                        return Ok(Some(MixedResolved::Chunk(raw, chunk)));
                    }
                    if let Some(row) =
                        storage.read_as_of(&scope, &fact_key(&scope, ulid), snapshot).await?
                        && let Ok(fact) =
                            serde_json::from_slice::<lunaris_extract::Fact>(&row.value)
                    {
                        // The at-rest Fact payload carries no `bt` — the gate
                        // rides the KV Row's bi-temporal stamp.
                        if sys_closed(&row.bt) {
                            return Ok(None); // soft-deleted fact row — skip
                        }
                        return Ok(Some(MixedResolved::Fact(raw, fact, row.bt)));
                    }
                    Ok(None) // neither row exists at this snapshot — skip
                }
            })
            .buffered(HYDRATE_CONCURRENCY)
            .collect()
            .await;
        results.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().flatten().collect()
    };

    // Second pass: batched episode-source lookups for the CHUNK hits only
    // (same shape as `hydrate`; storage errors propagate).
    let unique_ep: Vec<Ulid> = {
        let mut s = std::collections::HashSet::new();
        for r in &resolved {
            if let MixedResolved::Chunk(_, c) = r {
                s.insert(c.episode_id);
            }
        }
        s.into_iter().collect()
    };
    // Map ep_id -> (source, sys_closed) — same gate as `hydrate`: a chunk
    // whose parent episode was forgotten must not hydrate.
    let mut episode_sources: HashMap<Ulid, (String, bool)> = HashMap::new();
    let ep_results: Vec<Result<Option<(Ulid, String, bool)>, LunarisError>> =
        stream::iter(unique_ep)
            .map(|ep_id| {
                let scope = scope.clone();
                async move {
                    let key = episode_lookup_key(&scope, ep_id);
                    if let Some(row) = storage.read_as_of(&scope, &key, snapshot).await?
                        && let Ok(ep) = serde_json::from_slice::<Episode>(&row.value)
                    {
                        return Ok(Some((ep_id, ep.source, sys_closed(&ep.bt))));
                    }
                    Ok(None)
                }
            })
            .buffer_unordered(HYDRATE_CONCURRENCY)
            .collect()
            .await;
    for entry in ep_results.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().flatten() {
        episode_sources.insert(entry.0, (entry.1, entry.2));
    }

    // Third pass: project to Hits, dropping chunks of sys-closed episodes.
    Ok(resolved
        .into_iter()
        .filter(|r| match r {
            MixedResolved::Chunk(_, chunk) => {
                !matches!(episode_sources.get(&chunk.episode_id), Some((_, true)))
            }
            MixedResolved::Fact(..) => true, // fact rows gated at resolution
        })
        .map(|r| match r {
            MixedResolved::Chunk(raw, chunk) => Hit {
                id: raw.id,
                episode_id: chunk.episode_id.to_bytes().to_vec(),
                score: raw.score,
                text: chunk.text,
                source: episode_sources
                    .get(&chunk.episode_id)
                    .map(|(s, _)| s.clone())
                    .unwrap_or_default(),
                heading_path: chunk.heading_path,
                valid_from: chunk.bt.valid.0,
                valid_to: chunk.bt.valid.1,
                degraded: raw.degraded || initial_degraded,
                rerank_applied: raw.rerank_applied,
                source_op: raw.source_op,
            },
            MixedResolved::Fact(raw, fact, bt) => Hit {
                id: raw.id,
                // Facts have no parent Episode; the provenance channel stays
                // empty (documented "outside the main hydration path" case).
                episode_id: Vec::new(),
                score: raw.score,
                text: fact.fact_text,
                source: format!("fact:{}", fact.predicate),
                heading_path: Vec::new(),
                valid_from: bt.valid.0,
                valid_to: bt.valid.1,
                degraded: raw.degraded || initial_degraded,
                rerank_applied: raw.rerank_applied,
                source_op: raw.source_op,
            },
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
    scope: &Scope,
    hits: &[RawHit],
    as_of: Option<Hlc>,
) -> Result<HashMap<Vec<u8>, String>, LunarisError> {
    let live_clock = HlcClock::new(0);
    let snapshot = as_of.unwrap_or_else(|| live_clock.tick());

    // Plan 260610-f91: replace serial for loop with concurrent buffered fan-out.
    // Clone each hit's id upfront so the async block owns all its data
    // (avoids the higher-ranked lifetime bound on &RawHit closures).
    let owned: Vec<(Vec<u8>, Vec<u8>)> = hits
        .iter()
        .filter_map(|raw| chunk_lookup_key(scope, &raw.id).map(|key| (raw.id.clone(), key)))
        .collect();

    let pairs: Vec<(Vec<u8>, String)> = {
        #[allow(clippy::type_complexity)]
        let results: Vec<Result<Option<(Vec<u8>, String)>, LunarisError>> = stream::iter(owned)
            .map(|(id, key)| {
                let scope = scope.clone();
                async move {
                    match storage.read_as_of(&scope, &key, snapshot).await? {
                        Some(row) => {
                            if let Ok(chunk) = serde_json::from_slice::<Chunk>(&row.value) {
                                Ok(Some((id, chunk.text)))
                            } else {
                                Ok(None)
                            }
                        }
                        None => Ok(None), // chunk no longer exists — skip
                    }
                }
            })
            .buffered(HYDRATE_CONCURRENCY)
            .collect()
            .await;
        results.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().flatten().collect()
    };
    Ok(pairs.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_lookup_key_round_trips_ulid() {
        let id = Ulid::new();
        let bytes = id.to_bytes().to_vec();
        let scope = Scope::dev();
        let key = chunk_lookup_key(&scope, &bytes).unwrap();
        let s = String::from_utf8(key).unwrap();
        assert!(s.contains(":chunk:"));
        assert!(s.contains(&id.to_string()));
    }

    #[test]
    fn chunk_lookup_key_rejects_wrong_size() {
        let scope = Scope::dev();
        assert!(chunk_lookup_key(&scope, b"too-short").is_none());
    }
}
