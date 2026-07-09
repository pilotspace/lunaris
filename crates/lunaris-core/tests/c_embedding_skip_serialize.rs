//! W3 (moon-v051-perf-exploit) — embedding double-store fix.
//!
//! `Chunk::embedding`, `Entity::embedding`, `Fact::embedding`, and
//! `Community::summary_embedding` used to serialize the 768-d embedding as a
//! raw JSON float array inside the `KvPut` payload — a straight duplicate of
//! the binary vector Moon's FT index (and the dedicated vector column/table
//! in Postgres/SQLite) already stores, and ~80% of the document's bytes.
//! Nothing on any read path (`lunaris-retrieve::hydrate`,
//! `lunaris-retrieve::operators::tree` RAPTOR descent, the memory-inspector
//! `detail.rs` route, the SQLite `lunaris_vec` table, Postgres's `vector(768)`
//! column) reads `.embedding` back off a KV-deserialized primitive — verified
//! via `mcp__serena__find_referencing_symbols` before this cut.
//!
//! This suite proves, independent of any storage backend:
//! 1. The field is NEVER present in the serialized JSON (regardless of
//!    `Some`/`None`) — `c_embedding_never_serialized_*`.
//! 2. Deserialization tolerates BOTH shapes: legacy payloads written before
//!    this fix (field present) and payloads written after it (field absent)
//!    — `c_*_deserialize_tolerates_{legacy,new}_payload`.
//! 3. The measured payload-size reduction meets the ≥4× exit criterion for
//!    a realistic chunk — `c_chunk_kv_payload_shrinks_at_least_4x`.

use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::primitives::{Chunk, Community, Entity, Fact};
use lunaris_core::{HlcClock, Scope};
use ulid::Ulid;

fn scope() -> Scope {
    Scope::new("w3-embedding-fix").unwrap()
}

fn embedding_768() -> Vec<f32> {
    (0..768).map(|i| (i as f32) * 0.001234567 - 0.5).collect()
}

/// A real, correctly-shaped `bt` JSON value (avoids hand-guessing the
/// `(Hlc, Option<Hlc>)` wire shape for the legacy-payload fixtures below).
fn bt_json() -> serde_json::Value {
    let clock = HlcClock::new(0);
    serde_json::to_value(BiTemporal::now(&clock)).unwrap()
}

// ---------------------------------------------------------------------------
// 1. Never serialized (presence check via serde_json::Value, not the typed
//    struct — a typed round-trip would silently "fix itself" via #[serde(default)]
//    and hide a regression to skip_serializing_if instead of skip_serializing).
// ---------------------------------------------------------------------------

#[test]
fn c_chunk_embedding_never_serialized() {
    let clock = HlcClock::new(0);
    let mut c = Chunk::new(scope(), Ulid::new(), "hello world", 2, 0, vec!["H1".into()], &clock);
    c.embedding = Some(embedding_768());

    let v: serde_json::Value = serde_json::to_value(&c).unwrap();
    assert!(
        v.get("embedding").is_none(),
        "Chunk.embedding must never appear in the serialized JSON, even when Some"
    );
}

#[test]
fn c_entity_embedding_never_serialized() {
    let clock = HlcClock::new(0);
    let mut e = Entity::new(scope(), "Alice", "Person", 0.9, &clock);
    e.embedding = Some(embedding_768());

    let v: serde_json::Value = serde_json::to_value(&e).unwrap();
    assert!(
        v.get("embedding").is_none(),
        "Entity.embedding must never appear in the serialized JSON, even when Some"
    );
}

#[test]
fn c_fact_embedding_never_serialized() {
    let clock = HlcClock::new(0);
    let mut f =
        Fact::new(scope(), Ulid::new(), "joined", Ulid::new(), "Alice joined Acme", 0.95, &clock);
    f.embedding = Some(embedding_768());

    let v: serde_json::Value = serde_json::to_value(&f).unwrap();
    assert!(
        v.get("embedding").is_none(),
        "Fact.embedding must never appear in the serialized JSON, even when Some"
    );
}

#[test]
fn c_community_summary_embedding_never_serialized() {
    let clock = HlcClock::new(0);
    let mut c = Community::new(scope(), 0, "founders", &clock);
    c.summary_embedding = Some(embedding_768());

    let v: serde_json::Value = serde_json::to_value(&c).unwrap();
    assert!(
        v.get("summary_embedding").is_none(),
        "Community.summary_embedding must never appear in the serialized JSON, even when Some"
    );
}

// ---------------------------------------------------------------------------
// 2. Deserialize tolerance — legacy payloads (field present, written by a
//    pre-fix binary) and new payloads (field absent) must BOTH deserialize
//    cleanly. This is the backward/forward-compat contract for data already
//    on disk in production.
// ---------------------------------------------------------------------------

#[test]
fn c_chunk_deserialize_tolerates_legacy_payload_with_embedding() {
    // Hand-rolled JSON simulating a pre-fix KvPut payload that still carries
    // the field (old data in the wild MUST keep deserializing).
    let json = serde_json::json!({
        "id": Ulid::new().to_string(),
        "scope": "w3-embedding-fix",
        "episode_id": Ulid::new().to_string(),
        "text": "legacy chunk",
        "tokens": 2,
        "offset": 0,
        "heading_path": [],
        "overlap_tail": "",
        "embedding": [0.1, 0.2, 0.3],
        "parent_id": null,
        "bt": bt_json()
    });
    let c: Chunk = serde_json::from_value(json).expect("legacy payload must deserialize");
    assert_eq!(
        c.embedding,
        Some(vec![0.1, 0.2, 0.3]),
        "a legacy payload with the field present must still populate Some(..)"
    );
}

#[test]
fn c_chunk_deserialize_tolerates_new_payload_without_embedding() {
    let clock = HlcClock::new(0);
    let c = Chunk::new(scope(), Ulid::new(), "new chunk", 2, 0, vec![], &clock);
    // c.embedding defaults to None and is never serialized either way, but
    // the discriminating proof is deserializing a payload that has NO
    // "embedding" key at all (the post-fix on-disk shape).
    let mut v = serde_json::to_value(&c).unwrap();
    assert!(v.get("embedding").is_none(), "sanity: post-fix payload has no embedding key");
    // Explicitly ensure the key really is absent (defensive against a future
    // serde_json default that starts emitting nulls for Option fields).
    v.as_object_mut().unwrap().remove("embedding");
    let back: Chunk = serde_json::from_value(v).expect("field-absent payload must deserialize");
    assert_eq!(back.embedding, None, "field-absent payload must default to None");
}

#[test]
fn c_community_deserialize_tolerates_legacy_payload_with_summary_embedding() {
    let json = serde_json::json!({
        "id": Ulid::new().to_string(),
        "scope": "w3-embedding-fix",
        "level": 0,
        "parent": null,
        "members": [],
        "summary": "legacy community",
        "summary_embedding": [0.4, 0.5, 0.6],
        "bt": bt_json()
    });
    let c: Community = serde_json::from_value(json).expect("legacy payload must deserialize");
    assert_eq!(
        c.summary_embedding,
        Some(vec![0.4, 0.5, 0.6]),
        "a legacy Community payload with the field present must still populate Some(..)"
    );
}

// ---------------------------------------------------------------------------
// 3. Measured payload-size reduction — exit criterion: "KV payload size
//    reduced >= 4x on embedding-carrying kinds".
// ---------------------------------------------------------------------------

/// Reconstruct what the PRE-FIX serialized payload would have looked like by
/// serializing the struct (post-fix, so the field is skipped) and then
/// splicing the embedding back in as a JSON array — this measures the exact
/// byte delta the fix removes, without needing two copies of the struct
/// definition.
fn legacy_shaped_bytes<T: serde::Serialize>(
    value: &T,
    embedding_field: &str,
    emb: &[f32],
) -> Vec<u8> {
    let mut v = serde_json::to_value(value).unwrap();
    v.as_object_mut()
        .unwrap()
        .insert(embedding_field.to_string(), serde_json::to_value(emb).unwrap());
    serde_json::to_vec(&v).unwrap()
}

#[test]
fn c_chunk_kv_payload_shrinks_at_least_4x() {
    let clock = HlcClock::new(0);
    let mut c = Chunk::new(
        scope(),
        Ulid::new(),
        // Realistic short chunk body — the embedding dominates on real chunks
        // whose text is well under the 500-token chunker target.
        "The quarterly report shows revenue growth of 12% year over year.",
        14,
        0,
        vec!["H1".into(), "H2".into()],
        &clock,
    );
    let emb = embedding_768();
    c.embedding = Some(emb.clone());

    let post_fix_bytes = serde_json::to_vec(&c).unwrap();
    let pre_fix_bytes = legacy_shaped_bytes(&c, "embedding", &emb);

    let ratio = pre_fix_bytes.len() as f64 / post_fix_bytes.len() as f64;
    assert!(
        ratio >= 4.0,
        "expected >= 4x KV payload reduction for a realistic Chunk, got {:.2}x \
         (pre={} bytes, post={} bytes)",
        ratio,
        pre_fix_bytes.len(),
        post_fix_bytes.len()
    );
}

#[test]
fn c_entity_kv_payload_shrinks_at_least_4x() {
    let clock = HlcClock::new(0);
    let mut e = Entity::new(scope(), "Acme Corporation", "Organization", 0.92, &clock);
    let emb = embedding_768();
    e.embedding = Some(emb.clone());

    let post_fix_bytes = serde_json::to_vec(&e).unwrap();
    let pre_fix_bytes = legacy_shaped_bytes(&e, "embedding", &emb);

    let ratio = pre_fix_bytes.len() as f64 / post_fix_bytes.len() as f64;
    assert!(
        ratio >= 4.0,
        "expected >= 4x KV payload reduction for Entity, got {:.2}x (pre={} post={})",
        ratio,
        pre_fix_bytes.len(),
        post_fix_bytes.len()
    );
}

#[test]
fn c_fact_kv_payload_shrinks_at_least_4x() {
    let clock = HlcClock::new(0);
    let mut f = Fact::new(
        scope(),
        Ulid::new(),
        "acquired",
        Ulid::new(),
        "Acme Corporation acquired Widget Inc in Q3",
        0.88,
        &clock,
    );
    let emb = embedding_768();
    f.embedding = Some(emb.clone());

    let post_fix_bytes = serde_json::to_vec(&f).unwrap();
    let pre_fix_bytes = legacy_shaped_bytes(&f, "embedding", &emb);

    let ratio = pre_fix_bytes.len() as f64 / post_fix_bytes.len() as f64;
    assert!(
        ratio >= 4.0,
        "expected >= 4x KV payload reduction for Fact, got {:.2}x (pre={} post={})",
        ratio,
        pre_fix_bytes.len(),
        post_fix_bytes.len()
    );
}

#[test]
fn c_community_kv_payload_shrinks_at_least_4x() {
    let clock = HlcClock::new(0);
    let mut c = Community::new(
        scope(),
        1,
        "This section covers quarterly revenue and headcount growth.",
        &clock,
    );
    c.members = vec![Ulid::new(), Ulid::new(), Ulid::new()];
    let emb = embedding_768();
    c.summary_embedding = Some(emb.clone());

    let post_fix_bytes = serde_json::to_vec(&c).unwrap();
    let pre_fix_bytes = legacy_shaped_bytes(&c, "summary_embedding", &emb);

    let ratio = pre_fix_bytes.len() as f64 / post_fix_bytes.len() as f64;
    assert!(
        ratio >= 4.0,
        "expected >= 4x KV payload reduction for Community, got {:.2}x (pre={} post={})",
        ratio,
        pre_fix_bytes.len(),
        post_fix_bytes.len()
    );
}
