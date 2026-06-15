//! RED-first (memory-update-intelligence) — the `(subject, predicate)`
//! secondary index key used to detect cross-episode contradictions.
//!
//! References `lunaris_core::keyspace::fact_spo_key`, which does not exist yet
//! → compile-fail RED. Green lands in build batch 2.
//!
//! Note: the key takes the subject id as raw 16 bytes (`&[u8; 16]`), NOT the
//! `lunaris_extract::EntityId` type — `lunaris-core` must not depend on
//! `lunaris-extract` (layering). The caller passes `&entity_id.0`.

use lunaris_core::keyspace::fact_spo_key;
use lunaris_core::scope::Scope;

#[test]
fn fact_spo_key_is_scoped_stable_and_discriminating() {
    let scope = Scope::new("agent_a").expect("valid scope");
    let sid = [7u8; 16];

    let k1 = fact_spo_key(&scope, &sid, "employer");
    let k2 = fact_spo_key(&scope, &sid, "employer");
    assert_eq!(k1, k2, "same (scope, subject, predicate) must be stable");

    let s = String::from_utf8(k1.clone()).expect("utf-8 key");
    assert!(
        s.starts_with("lunaris:agent_a:factspo:"),
        "key must be scope-prefixed under the factspo kind; got {s}"
    );
    assert!(s.ends_with(":employer"), "predicate must terminate the key; got {s}");

    // Different predicate / different subject → different index row.
    assert_ne!(k1, fact_spo_key(&scope, &sid, "founder"));
    assert_ne!(k1, fact_spo_key(&scope, &[8u8; 16], "employer"));
}

#[test]
fn fact_spo_key_is_scope_isolated() {
    let a = Scope::new("scope_a").unwrap();
    let b = Scope::new("scope_b").unwrap();
    let sid = [1u8; 16];
    assert_ne!(
        fact_spo_key(&a, &sid, "employer"),
        fact_spo_key(&b, &sid, "employer"),
        "the same (subject, predicate) in different scopes must not byte-alias"
    );
}
