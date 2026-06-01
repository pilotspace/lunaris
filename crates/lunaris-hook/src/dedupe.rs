//! Deterministic blake3 dedupe key derivation (HOOK-05).
//!
//! ## Algorithm (locked in CONTEXT.md §Grey area 3)
//!
//! 1. Build canonical JSON of the post-scrub envelope value (sorted keys, no whitespace).
//! 2. Concatenate: `scope_bytes || 0x1F || event_id_bytes || 0x1F || canonical_json_bytes`.
//! 3. Hash with blake3. Hex-encode (64 chars).
//!
//! `event_id` fallback: `blake3_hex16(session_id || hook_event_name || transcript_path || timestamp)`.
//! Returns 32 hex chars (first 16 bytes of blake3 — enough for deterministic synthesis
//! while staying compact in the storage row).
//!
//! ## Why canonical JSON
//!
//! Map key ordering in `serde_json::Value` depends on insertion order. Two
//! semantically-identical envelopes that differ only in key order would produce
//! different hashes without this normalisation step. Sorting keys recursively
//! guarantees that the dedupe key is stable across:
//! - Different Claude Code versions that emit the same fields in a different order.
//! - Re-serialise-and-deserialise round-trips that change map order.
//!
//! ## Why blake3
//!
//! `lunaris-hook` already depends on `blake3` (scope derivation). Adding a
//! second hash function for dedupe would inflate the binary; blake3's 256-bit
//! output provides 128-bit collision resistance — sufficient for a per-scope
//! event deduplication table.

use serde_json::Value;

/// Canonical JSON encoding of a `serde_json::Value`.
///
/// Map keys are sorted recursively (depth-first). Output is compact (no whitespace).
/// Arrays preserve their element order — only Object maps are sorted.
/// Returns the UTF-8 bytes of the resulting JSON string.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let canonical = sort_value(value.clone());
    serde_json::to_vec(&canonical).expect("canonical_json: serialization of a Value cannot fail")
}

/// Recursively sort all Object maps by key (lexicographic, Unicode code-point order).
fn sort_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: serde_json::Map<String, Value> = serde_json::Map::new();
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            for k in keys {
                // `map[&k]` is fine: we collected the keys from the same map.
                let v = map[&k].clone();
                sorted.insert(k, sort_value(v));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_value).collect()),
        other => other,
    }
}

/// Derive a synthetic event_id when the envelope carries none.
///
/// Hashes: `session_id || hook_event_name || transcript_path || timestamp`.
/// `transcript_path` and `timestamp` are included when present so that two
/// different sessions that happen to share a `session_id` (e.g. short test
/// sessions) don't collide.
///
/// Returns a 32-character lowercase hex string (first 16 bytes of blake3).
/// 32 hex chars = 16 bytes = 128 bits of entropy — sufficient to distinguish
/// hook invocations within a session without storing the full 64-char key.
pub fn derive_event_id(
    session_id: &str,
    hook_event_name: &str,
    transcript_path: Option<&str>,
    timestamp: Option<&str>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(session_id.as_bytes());
    hasher.update(hook_event_name.as_bytes());
    if let Some(tp) = transcript_path {
        hasher.update(tp.as_bytes());
    }
    if let Some(ts) = timestamp {
        hasher.update(ts.as_bytes());
    }
    let hash = hasher.finalize();
    // First 16 bytes → 32 hex chars.
    hex::encode(&hash.as_bytes()[..16])
}

/// Derive the canonical dedupe key for a (scope, event_id, canonical_json) triple.
///
/// Concatenates fields with ASCII Unit Separator (0x1F) as the delimiter.
/// This byte cannot appear in valid UTF-8 scope strings or JSON, so it provides
/// unambiguous field separation without encoding overhead.
///
/// Returns a 64-character lowercase hex string (full 256-bit blake3 output).
pub fn derive_dedupe_key(scope: &str, event_id: &str, canonical_json_bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(scope.as_bytes());
    hasher.update(&[0x1F]); // ASCII Unit Separator
    hasher.update(event_id.as_bytes());
    hasher.update(&[0x1F]);
    hasher.update(canonical_json_bytes);
    hex::encode(hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_empty_object() {
        let v = serde_json::json!({});
        assert_eq!(canonical_json(&v), b"{}");
    }

    #[test]
    fn canonical_json_sorted_keys() {
        let mut map = serde_json::Map::new();
        map.insert("z".to_string(), serde_json::json!(1));
        map.insert("a".to_string(), serde_json::json!(2));
        let bytes = canonical_json(&Value::Object(map));
        let s = std::str::from_utf8(&bytes).unwrap();
        // "a" must appear before "z"
        assert!(s.find("\"a\"").unwrap() < s.find("\"z\"").unwrap());
    }

    #[test]
    fn derive_dedupe_key_length() {
        let v = serde_json::json!({"x": 1});
        let cb = canonical_json(&v);
        let k = derive_dedupe_key("s", "e", &cb);
        assert_eq!(k.len(), 64);
    }

    #[test]
    fn derive_event_id_length() {
        let id = derive_event_id("sid", "PreToolUse", Some("/tmp"), Some("2026-01-01T00:00:00Z"));
        assert_eq!(id.len(), 32);
    }
}
