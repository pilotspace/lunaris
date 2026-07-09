//! Serde roundtrip + bi-temporal stamp + Send/Sync proofs for all six primitives.
//!
//! RFC 0001 Wave 1A: dev-scope migration crutch replaced with real validated scopes.

use lunaris_core::*;
use ulid::Ulid;

fn assert_send_sync_static<T: Send + Sync + 'static>() {}

#[test]
fn all_six_are_send_sync_static() {
    assert_send_sync_static::<Episode>();
    assert_send_sync_static::<Chunk>();
    assert_send_sync_static::<Entity>();
    assert_send_sync_static::<Relation>();
    assert_send_sync_static::<Fact>();
    assert_send_sync_static::<Community>();
}

#[test]
fn primitives_carry_bitemporal_stamp_on_construct() {
    let clock = HlcClock::new(0);
    let ep = Episode::new(Scope::new("test-scope").unwrap(), "notes.md", "hello", &clock);
    let later = clock.tick();
    assert!(ep.bt.valid_at(later), "Episode bt should include later timestamps");
    assert!(ep.bt.system_at(later));
}

#[test]
fn episode_roundtrip() {
    let clock = HlcClock::new(0);
    let ep = Episode::new(Scope::new("test-scope").unwrap(), "notes.md", "hello world", &clock);
    let s = serde_json::to_string(&ep).unwrap();
    let back: Episode = serde_json::from_str(&s).unwrap();
    assert_eq!(ep, back);
}

#[test]
fn chunk_roundtrip() {
    let clock = HlcClock::new(0);
    let mut c = Chunk::new(
        Scope::new("test-scope").unwrap(),
        Ulid::new(),
        "hi",
        1,
        0,
        vec!["H1".into()],
        &clock,
    );
    c.embedding = Some(vec![0.1, 0.2, 0.3]);
    c.overlap_tail = "...".into();
    let s = serde_json::to_string(&c).unwrap();
    let back: Chunk = serde_json::from_str(&s).unwrap();
    // W3 embedding double-store fix: `embedding` is skip_serializing (the
    // binary vector lives in the FT index / vector column, never the KV JSON
    // payload), so it is intentionally LOST across a serde roundtrip. Assert
    // that contract explicitly, then compare everything else.
    assert_eq!(back.embedding, None, "embedding must not survive serialization (skip_serializing)");
    let mut expect = c.clone();
    expect.embedding = None;
    assert_eq!(expect, back);
}

#[test]
fn entity_roundtrip() {
    let clock = HlcClock::new(0);
    let mut e = Entity::new(Scope::new("test-scope").unwrap(), "Alice", "Person", 0.9, &clock);
    e.aliases = vec!["Al".into()];
    let s = serde_json::to_string(&e).unwrap();
    let back: Entity = serde_json::from_str(&s).unwrap();
    assert_eq!(e, back);
}

#[test]
fn relation_roundtrip() {
    let clock = HlcClock::new(0);
    let r = Relation::new(
        Scope::new("test-scope").unwrap(),
        Ulid::new(),
        Ulid::new(),
        "WORKS_AT",
        0.8,
        &clock,
    );
    let s = serde_json::to_string(&r).unwrap();
    let back: Relation = serde_json::from_str(&s).unwrap();
    assert_eq!(r, back);
}

#[test]
fn fact_roundtrip() {
    let clock = HlcClock::new(0);
    let f = Fact::new(
        Scope::new("test-scope").unwrap(),
        Ulid::new(),
        "joined",
        Ulid::new(),
        "Alice joined Acme",
        0.95,
        &clock,
    );
    let s = serde_json::to_string(&f).unwrap();
    let back: Fact = serde_json::from_str(&s).unwrap();
    assert_eq!(f, back);
}

#[test]
fn community_roundtrip() {
    let clock = HlcClock::new(0);
    let mut c = Community::new(Scope::new("test-scope").unwrap(), 0, "founders", &clock);
    c.members = vec![Ulid::new(), Ulid::new()];
    let s = serde_json::to_string(&c).unwrap();
    let back: Community = serde_json::from_str(&s).unwrap();
    assert_eq!(c, back);
}
