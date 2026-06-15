//! Extractor DTOs — [`EntityId`] content-hash + [`Entity`] / [`Relation`] /
//! [`Fact`] / [`ChunkInput`] / [`RawExtraction`] / [`RawExtractionBatch`] /
//! [`ExtractionBatch`].
//!
//! These are the extractor-side DTOs that flow through
//! [`crate::Extractor::extract`] → [`crate::validator::validate`] → Plan 03-03
//! ingest fan-out. They are intentionally distinct from
//! [`lunaris_core::Entity`] / [`lunaris_core::Relation`] / [`lunaris_core::Fact`]
//! (which use [`Ulid`] ids) because:
//!
//! - The extractor produces **content-hash IDs** (D-06: `EntityId =
//!   blake3(canonical_name_normalized || "::" || entity_type)[..16]`) so
//!   re-ingest of the same entity yields the same id without a round trip.
//! - Plan 03-03 maps `EntityId` → `Ulid` at the WriteOp boundary via
//!   `Ulid::from_bytes(entity_id.0)` (16 bytes ↔ 16 bytes is a clean cast).
//!
//! Keeping the two type families separate makes the conversion site explicit
//! and lets the extractor evolve its DTO shape without churning the locked
//! Phase 1 primitives.

use std::convert::TryInto;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Deterministic 16-byte content-hash identifier per D-06.
///
/// ```text
/// EntityId = blake3(canonical_name_normalized || "::" || entity_type)[..16]
/// ```
///
/// Where `canonical_name_normalized` is:
/// 1. lowercase
/// 2. trim leading + trailing whitespace
/// 3. collapse internal runs of whitespace to a single space
/// 4. drop trailing punctuation `.` `,` `;` `:` `!` `?`
///
/// 16 bytes = 128 bits of entropy — collision probability ≈ 2^-64 for ~1B
/// entities (birthday bound). Cross-Episode disambiguation (e.g., "Alice" in
/// Episode A vs "Alice Smith" in Episode B referring to the same person) is
/// D-09 deferred to the Phase 4 Verifier; v0 keeps them as distinct EntityIds
/// — the conservative default per blueprint §5.2 "graph default-off".
///
/// `Display` renders the lowercase hex; useful for tracing logs and Cypher
/// `WHERE n.id = '...'` literals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId(pub [u8; 16]);

impl EntityId {
    /// Derive a deterministic [`EntityId`] from `(name, entity_type)`.
    ///
    /// See struct rustdoc for the canonicalization steps. The output is byte-
    /// identical across calls for the same input (proven by
    /// `entity_id_is_deterministic` test) and treats `("Alice  Smith.  ",
    /// "Person")` and `("alice smith", "Person")` as the same id (proven by
    /// `entity_id_normalizes_whitespace_and_punctuation` test). The
    /// `entity_type` is appended verbatim (no normalization) so `("Alice",
    /// "Person")` and `("Alice", "Place")` stay distinct (proven by
    /// `entity_id_distinguishes_by_type` test).
    pub fn from_name_and_type(name: &str, entity_type: &str) -> Self {
        let normalized = canonicalize_name(name);
        let key = format!("{normalized}::{entity_type}");
        let h = blake3::hash(key.as_bytes());
        let bytes: [u8; 16] =
            h.as_bytes()[..16].try_into().expect("blake3 output is 32 bytes; first 16 always fit");
        EntityId(bytes)
    }

    /// Decode an [`EntityId`] from its 32-char lowercase-hex `Display` form.
    ///
    /// Returns `None` if `s` is not exactly 32 ASCII hex chars. This is the
    /// inverse of `Display` and is the round-trip path used by `Graph::anchored`
    /// to map the Cypher-emitted `source_entity_id` (rendered with the same
    /// `format!("{}", id)` shape on the write path) back to the seed key
    /// for confidence lookups.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        hex::decode_to_slice(s, &mut bytes).ok()?;
        Some(EntityId(bytes))
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// Deterministic 16-byte content-hash identifier for a `(subject, predicate,
/// object)` fact triple — the **sync-dedup key** for memory-update convergence.
///
/// ```text
/// FactId = blake3(subject_id.0 || 0x1F || predicate || 0x1F || object_id.0)[..16]
/// ```
///
/// `0x1F` (ASCII Unit Separator) delimits the three fields so a predicate that
/// happens to contain the byte sequence of an adjacent id cannot byte-alias
/// into a neighbouring triple's key. Mirrors [`EntityId::from_name_and_type`]:
/// re-asserting the identical triple yields the same `FactId`, so the
/// structured-ingest write path can mint a stable `Ulid::from_bytes(fact_id.0)`
/// and an exact re-ingest overwrites the same row in place (idempotent NOOP)
/// rather than accruing a duplicate fact. A different object OR predicate yields
/// a different id (a value change is NOT a dup) — proven by
/// `factid_from_triple_is_deterministic_and_distinct`.
///
/// 16 bytes ↔ `Ulid` (both 128-bit) is a clean cast at the WriteOp boundary,
/// same as the `EntityId` → `Ulid` mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FactId(pub [u8; 16]);

impl FactId {
    /// Derive the deterministic [`FactId`] for a `(subject_id, predicate,
    /// object_id)` triple. See the struct rustdoc for the formula and rationale.
    pub fn from_triple(subject_id: EntityId, predicate: &str, object_id: EntityId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&subject_id.0);
        hasher.update(&[0x1F]);
        hasher.update(predicate.as_bytes());
        hasher.update(&[0x1F]);
        hasher.update(&object_id.0);
        let h = hasher.finalize();
        let bytes: [u8; 16] =
            h.as_bytes()[..16].try_into().expect("blake3 output is 32 bytes; first 16 always fit");
        FactId(bytes)
    }
}

impl std::fmt::Display for FactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// Canonicalize an entity name per D-06:
/// lowercase + trim + collapse internal whitespace + drop trailing punctuation.
///
/// Pulled out of [`EntityId::from_name_and_type`] for unit-test clarity.
fn canonicalize_name(raw: &str) -> String {
    // 1. lowercase + 2. trim leading/trailing whitespace
    let lower = raw.to_lowercase();
    let trimmed = lower.trim();
    // 3. collapse internal whitespace runs to single space
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_was_ws = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_was_ws {
                out.push(' ');
                prev_was_ws = true;
            }
        } else {
            out.push(ch);
            prev_was_ws = false;
        }
    }
    // 4. drop trailing punctuation (D-06 spec: `.` `,` `;` `:` `!` `?`)
    while let Some(last) = out.chars().last() {
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?') {
            out.pop();
        } else {
            break;
        }
    }
    // re-trim in case punctuation drop exposed trailing whitespace
    out.trim_end().to_string()
}

// ----------------------------- Chunk input -----------------------------------

/// What [`crate::Extractor::extract`] consumes. The Plan 03-03 ingest fan-out
/// builds these from `lunaris_core::Chunk` after the markdown chunker runs.
///
/// `chunk_id` is the deterministic chunk Ulid from Plan 02-01; it flows back
/// out as [`RawExtraction::source_chunk_id`] so the validator + ingest fan-out
/// can route extractions back to their source chunk for provenance.
///
/// `heading_path` is preserved from the chunker output so the prompt can
/// surface the document section to the model (improves entity disambiguation
/// when the same name means different things in different sections).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkInput {
    pub chunk_id: Ulid,
    pub text: String,
    #[serde(default)]
    pub heading_path: Vec<String>,
}

// ------------------------------- Entity --------------------------------------

/// Extractor-side entity DTO. Carries the deterministic content-hash
/// [`EntityId`] (D-06) instead of the [`Ulid`] used by [`lunaris_core::Entity`].
///
/// Plan 03-03 maps `EntityId` → `Ulid` at the WriteOp boundary via
/// `Ulid::from_bytes(entity_id.0)`. The 16-byte ↔ 16-byte cast is lossless.
///
/// `valid_from_iso` / `valid_to_iso` are RFC3339 strings — the validator's
/// bi-temporal sanity check uses lexicographic comparison which is correct
/// when both timestamps share the same UTC offset (as RFC3339 produces by
/// convention with `Z`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub entity_type: String,
    pub confidence: f32,
    pub valid_from_iso: String,
    #[serde(default)]
    pub valid_to_iso: Option<String>,
}

// ------------------------------ Relation -------------------------------------

/// Extractor-side relation DTO. Per D-07 relations reference [`EntityId`]s
/// directly — no separate id-resolution pass.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub subject_id: EntityId,
    pub predicate: String,
    pub object_id: EntityId,
    pub confidence: f32,
    pub valid_from_iso: String,
    #[serde(default)]
    pub valid_to_iso: Option<String>,
}

// -------------------------------- Fact ---------------------------------------

/// Extractor-side fact DTO. The fact's natural-language text lives alongside
/// the structured `(subject, predicate, object)` triple so downstream callers
/// can render the fact verbatim in retrieval results without a round trip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub id: Ulid,
    pub subject_id: EntityId,
    pub predicate: String,
    pub object_id: EntityId,
    pub fact_text: String,
    pub confidence: f32,
    pub valid_from_iso: String,
    #[serde(default)]
    pub valid_to_iso: Option<String>,
}

// ------------------------------- Batches -------------------------------------

/// One chunk's raw extraction before validation. Preserves the source
/// `chunk_id` so the Plan 03-03 fan-out can attach provenance per-extraction.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RawExtraction {
    pub source_chunk_id: Ulid,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub relations: Vec<Relation>,
    #[serde(default)]
    pub facts: Vec<Fact>,
}

/// What [`crate::Extractor::extract`] returns. Holds one [`RawExtraction`] per
/// input chunk, in input order, so the caller can correlate by index when
/// needed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RawExtractionBatch {
    pub by_chunk: Vec<RawExtraction>,
}

/// Flattened, post-validation extraction. The Plan 03-03 ingest fan-out
/// converts a [`crate::ValidatedExtraction`] into this shape via
/// [`crate::into_batch`] for direct consumption by the WriteOp builder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtractionBatch {
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
    pub facts: Vec<Fact>,
}

// ------------------------------- Tests ---------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_id_is_deterministic() {
        let a = EntityId::from_name_and_type("Alice Smith", "Person");
        let b = EntityId::from_name_and_type("Alice Smith", "Person");
        assert_eq!(a, b, "same input MUST produce byte-identical EntityId");
        // Display roundtrip — hex encoding stable across calls
        assert_eq!(a.to_string(), b.to_string());
        assert_eq!(a.to_string().len(), 32, "16 bytes → 32 hex chars");
    }

    #[test]
    fn entity_id_normalizes_whitespace_and_punctuation() {
        // D-06 canonicalization spec:
        // lowercase + trim + collapse-whitespace + drop-trailing-punct
        let a = EntityId::from_name_and_type("alice smith", "Person");
        let b = EntityId::from_name_and_type("Alice  Smith.  ", "Person");
        let c = EntityId::from_name_and_type("ALICE\t SMITH!", "Person");
        let d = EntityId::from_name_and_type("  alice smith ?", "Person");
        assert_eq!(a, b, "internal-whitespace + trailing-period must canonicalize");
        assert_eq!(a, c, "tabs + uppercase + trailing-bang must canonicalize");
        assert_eq!(a, d, "leading/trailing space + trailing-question must canonicalize");
    }

    #[test]
    fn entity_id_distinguishes_by_type() {
        let person = EntityId::from_name_and_type("Alice", "Person");
        let place = EntityId::from_name_and_type("Alice", "Place");
        assert_ne!(person, place, "(name, type) pair must matter for id");
    }

    #[test]
    fn entity_id_distinguishes_different_names() {
        let alice = EntityId::from_name_and_type("Alice", "Person");
        let bob = EntityId::from_name_and_type("Bob", "Person");
        assert_ne!(alice, bob);
    }

    #[test]
    fn entity_id_from_hex_roundtrips_display() {
        let id = EntityId::from_name_and_type("Alice", "Person");
        let hex = format!("{}", id);
        let decoded = EntityId::from_hex(&hex).expect("valid 32-char hex must decode");
        assert_eq!(id, decoded, "from_hex MUST be the inverse of Display");
    }

    #[test]
    fn entity_id_from_hex_rejects_malformed() {
        // Wrong length (33 chars instead of 32).
        assert_eq!(EntityId::from_hex(&"a".repeat(33)), None);
        // Wrong length (31 chars).
        assert_eq!(EntityId::from_hex(&"a".repeat(31)), None);
        // Right length, non-hex chars.
        assert_eq!(EntityId::from_hex(&"z".repeat(32)), None);
        // Empty string.
        assert_eq!(EntityId::from_hex(""), None);
    }

    #[test]
    fn entity_id_display_is_lowercase_hex() {
        let id = EntityId([
            0xAB, 0xCD, 0xEF, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
            0x0C, 0x0D,
        ]);
        assert_eq!(id.to_string(), "abcdef0102030405060708090a0b0c0d");
    }

    #[test]
    fn canonicalize_handles_empty_and_only_punct() {
        // Defensive — extractor backends should never emit empty names but
        // the validator enforces it at the next stage. Here we just prove
        // canonicalization doesn't panic.
        assert_eq!(canonicalize_name(""), "");
        assert_eq!(canonicalize_name("   "), "");
        assert_eq!(canonicalize_name("!?.,"), "");
    }

    #[test]
    fn entity_id_to_ulid_bytes_are_lossless() {
        // Plan 03-03 maps EntityId → Ulid at the WriteOp boundary via
        // `Ulid::from_bytes(entity_id.0)`. Prove the round trip is lossless.
        let id = EntityId::from_name_and_type("Alice", "Person");
        let as_ulid = Ulid::from_bytes(id.0);
        let bytes_back: [u8; 16] = as_ulid.to_bytes();
        assert_eq!(id.0, bytes_back);
    }

    #[test]
    fn raw_extraction_serde_roundtrip() {
        let raw = RawExtraction {
            source_chunk_id: Ulid::new(),
            entities: vec![Entity {
                id: EntityId::from_name_and_type("Alice", "Person"),
                name: "Alice".into(),
                aliases: vec!["Al".into()],
                entity_type: "Person".into(),
                confidence: 0.92,
                valid_from_iso: "2024-01-01T00:00:00Z".into(),
                valid_to_iso: None,
            }],
            relations: vec![],
            facts: vec![],
        };
        let json = serde_json::to_string(&raw).unwrap();
        let parsed: RawExtraction = serde_json::from_str(&json).unwrap();
        assert_eq!(raw, parsed);
    }
}
