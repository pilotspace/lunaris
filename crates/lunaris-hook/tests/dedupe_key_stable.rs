//! TDD RED: Dedupe key stability tests for HOOK-05.
//!
//! These tests FAIL at compile time until `crates/lunaris-hook/src/dedupe.rs`
//! is created with the correct public API. That is the expected RED-gate state.
//!
//! Tests document the locked algorithm from CONTEXT.md:
//!   dedupe_key = blake3_hex(scope_bytes || 0x1F || event_id_bytes || 0x1F || canonical_json_bytes)
//!   canonical_json = sort map keys recursively, no whitespace
//!   event_id fallback = blake3_hex16(session_id || hook_event_name || transcript_path || timestamp)

use lunaris_hook::dedupe::{canonical_json, derive_dedupe_key, derive_event_id};

/// Test 1: Same input → same 64-char hex key across two calls (key stability).
#[test]
fn key_is_stable_same_inputs() {
    let value = serde_json::json!({"tool_name": "Edit", "path": "src/main.rs"});
    let canonical = canonical_json(&value);
    let key1 = derive_dedupe_key("scope-a", "evt-001", &canonical);
    let key2 = derive_dedupe_key("scope-a", "evt-001", &canonical);
    assert_eq!(key1, key2, "same inputs must produce the same dedupe key");
}

/// Test 2: Mutating one byte in the JSON value → different key.
#[test]
fn mutation_produces_different_key() {
    let value_orig = serde_json::json!({"tool_name": "Edit", "path": "src/main.rs"});
    let value_mut = serde_json::json!({"tool_name": "Edit", "path": "src/lib.rs"});
    let canonical_orig = canonical_json(&value_orig);
    let canonical_mut = canonical_json(&value_mut);
    let key_orig = derive_dedupe_key("scope-a", "evt-001", &canonical_orig);
    let key_mut = derive_dedupe_key("scope-a", "evt-001", &canonical_mut);
    assert_ne!(key_orig, key_mut, "mutated content must produce a different key");
}

/// Test 3: Same JSON + same event_id, different scope → different key.
#[test]
fn different_scope_produces_different_key() {
    let value = serde_json::json!({"x": 1});
    let canonical = canonical_json(&value);
    let key_a = derive_dedupe_key("scope-a", "evt-001", &canonical);
    let key_b = derive_dedupe_key("scope-b", "evt-001", &canonical);
    assert_ne!(key_a, key_b, "different scopes must produce different keys");
}

/// Test 4: Same JSON + same scope, different event_id → different key.
#[test]
fn different_event_id_produces_different_key() {
    let value = serde_json::json!({"x": 1});
    let canonical = canonical_json(&value);
    let key_a = derive_dedupe_key("scope-a", "evt-001", &canonical);
    let key_b = derive_dedupe_key("scope-a", "evt-002", &canonical);
    assert_ne!(key_a, key_b, "different event_ids must produce different keys");
}

/// Test 5: Map key insertion order does NOT affect canonical_json output.
/// `{"b":2,"a":1}` and `{"a":1,"b":2}` must produce the same canonical bytes.
#[test]
fn canonical_json_map_order_independence() {
    // serde_json preserves insertion order in Objects; build two maps with opposite insertion order
    let mut map1 = serde_json::Map::new();
    map1.insert("b".to_string(), serde_json::json!(2));
    map1.insert("a".to_string(), serde_json::json!(1));
    let value1 = serde_json::Value::Object(map1);

    let mut map2 = serde_json::Map::new();
    map2.insert("a".to_string(), serde_json::json!(1));
    map2.insert("b".to_string(), serde_json::json!(2));
    let value2 = serde_json::Value::Object(map2);

    assert_eq!(
        canonical_json(&value1),
        canonical_json(&value2),
        "canonical_json must produce identical bytes regardless of map key insertion order"
    );
}

/// Test 6: Nested map keys are sorted recursively.
/// `{"x":{"z":3,"y":2},"w":1}` → outer key `w` before `x`, inner key `y` before `z`.
#[test]
fn canonical_json_nested_map_sort() {
    let mut inner_unsorted = serde_json::Map::new();
    inner_unsorted.insert("z".to_string(), serde_json::json!(3));
    inner_unsorted.insert("y".to_string(), serde_json::json!(2));

    let mut outer_unsorted = serde_json::Map::new();
    outer_unsorted.insert("x".to_string(), serde_json::Value::Object(inner_unsorted));
    outer_unsorted.insert("w".to_string(), serde_json::json!(1));

    let canonical_bytes = canonical_json(&serde_json::Value::Object(outer_unsorted));
    let canonical_str =
        std::str::from_utf8(&canonical_bytes).expect("canonical JSON must be valid UTF-8");

    // Outer: "w" must appear before "x" in the serialized form
    let w_pos = canonical_str.find("\"w\"").expect("key w must be present");
    let x_pos = canonical_str.find("\"x\"").expect("key x must be present");
    assert!(w_pos < x_pos, "outer key 'w' must sort before 'x'");

    // Inner: "y" must appear before "z"
    let y_pos = canonical_str.find("\"y\"").expect("key y must be present");
    let z_pos = canonical_str.find("\"z\"").expect("key z must be present");
    assert!(y_pos < z_pos, "inner key 'y' must sort before 'z'");
}

/// Test 7: Envelope with no event_id → `derive_event_id` returns deterministic 32-char hex.
#[test]
fn derive_event_id_is_deterministic() {
    let id1 = derive_event_id(
        "session-abc",
        "PreToolUse",
        Some("/home/user/.claude/projects/foo"),
        Some("2026-05-25T00:00:00Z"),
    );
    let id2 = derive_event_id(
        "session-abc",
        "PreToolUse",
        Some("/home/user/.claude/projects/foo"),
        Some("2026-05-25T00:00:00Z"),
    );
    assert_eq!(id1, id2, "derive_event_id must be deterministic for same inputs");
    assert_eq!(id1.len(), 32, "derive_event_id must return a 32-char hex string (16 bytes)");
    assert!(
        id1.chars().all(|c| c.is_ascii_hexdigit()),
        "derive_event_id must return a valid hex string"
    );
}

/// Test 8: Dedupe key is always 64 hex chars (256-bit blake3 output).
#[test]
fn key_length_is_always_64() {
    let value = serde_json::json!({"hello": "world"});
    let canonical = canonical_json(&value);
    let key = derive_dedupe_key("test-scope", "test-event-id", &canonical);
    assert_eq!(key.len(), 64, "dedupe key must always be 64 hex chars (256-bit blake3)");
    assert!(
        key.chars().all(|c| c.is_ascii_hexdigit()),
        "dedupe key must be a valid lowercase hex string"
    );
}
