//! Schema gate — invariant enforcement for `VectorUpsert` chunk metadata.
//!
//! ## Purpose
//!
//! Generalises the B4 inline hotfix at `pipeline.rs:95-104` (commit `06063c95`)
//! into a reusable, testable contract.  Every `WriteOp::VectorUpsert` whose
//! `index == "chunks"` MUST carry non-empty `"text"` metadata so that both the
//! Postgres BM25 backend (`payload->>'text'` per migration `20260421_000004`)
//! and the Moon BM25/HYBRID backend (`extract_content_for_index`) can score the
//! chunk.  Without `"text"` both backends silently return zero hits even when
//! the query tokens are present verbatim in the chunk body.
//!
//! ## Usage
//!
//! In any code path that builds `VectorUpsert { metadata, .. }` for the
//! `"chunks"` index, call **before** pushing the op:
//!
//! ```rust,ignore
//! validate_chunk_metadata(&metadata)
//!     .map_err(|e| LunarisError::Storage(StorageError::Backend(format!("schema gate: {e}"))))?;
//! ```
//!
//! Or, if you already hold the text value as a `&str`:
//!
//! ```rust,ignore
//! validate_chunk_text(chunk.text.as_str())
//!     .map_err(|e| LunarisError::Storage(StorageError::Backend(format!("schema gate: {e}"))))?;
//! ```
//!
//! See [Gap 9 / L9] in the Lunaris integration plan and
//! `feedback_helios_memories_chunk_text.md` for incident background.

use thiserror::Error;

/// Errors produced by the chunk-metadata schema gate.
///
/// These are deliberately typed so callers can match on specific failure
/// modes in tests; at the `ingest_episode` boundary they are mapped into
/// `LunarisError::Storage(StorageError::Backend(...))`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SchemaError {
    /// A required field is absent from the metadata object.
    #[error("chunk metadata is missing required field '{field}'")]
    MissingField {
        /// Name of the absent field.
        field: &'static str,
    },

    /// A required field is present but its string value is empty (`""`).
    ///
    /// Note: whitespace-only values are **not** rejected — the chunker
    /// will never produce them and over-eager rejection would block
    /// intentional minimal whitespace content.
    #[error("chunk metadata field '{field}' must not be empty")]
    EmptyField {
        /// Name of the field whose value is an empty string.
        field: &'static str,
    },

    /// A required field is present but has the wrong JSON type (e.g. a
    /// `null` or an integer where a string is expected).
    #[error("chunk metadata field '{field}' has wrong type; expected {expected}")]
    WrongType {
        /// Name of the field with the wrong type.
        field: &'static str,
        /// Human-readable description of the expected type (e.g. `"non-null string"`).
        expected: &'static str,
    },
}

/// Validate that a chunk-metadata [`serde_json::Value`] contains a non-empty
/// `"text"` string field.
///
/// This is the primary guard called in `pipeline.rs` before every
/// `ops.push(WriteOp::VectorUpsert { ... })` for the `"chunks"` index.
///
/// # Errors
///
/// | Condition | Variant |
/// |---|---|
/// | `"text"` key absent | [`SchemaError::MissingField`] |
/// | `"text"` is `null` or non-string JSON value | [`SchemaError::WrongType`] |
/// | `"text"` is an empty string `""` | [`SchemaError::EmptyField`] |
///
/// # Performance
///
/// O(1) — a single `Value::get` hash lookup + type tag check + `is_empty`
/// on the existing `str` slice.  No allocations on the happy path.
pub fn validate_chunk_metadata(metadata: &serde_json::Value) -> Result<(), SchemaError> {
    match metadata.get("text") {
        None => Err(SchemaError::MissingField { field: "text" }),
        Some(serde_json::Value::String(s)) => {
            if s.is_empty() {
                Err(SchemaError::EmptyField { field: "text" })
            } else {
                Ok(())
            }
        }
        Some(_) => Err(SchemaError::WrongType { field: "text", expected: "non-null string" }),
    }
}

/// Validate a chunk text value already extracted as a `&str`.
///
/// Prefer this over [`validate_chunk_metadata`] when the text is available
/// before the metadata object is constructed (saves one `serde_json::Value`
/// allocation).
///
/// # Errors
///
/// Returns [`SchemaError::EmptyField`] if `text` is an empty string.
pub fn validate_chunk_text(text: &str) -> Result<(), SchemaError> {
    if text.is_empty() {
        Err(SchemaError::EmptyField { field: "text" })
    } else {
        Ok(())
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── validate_chunk_metadata ───────────────────────────────────────────────

    #[test]
    fn valid_metadata_passes() {
        let meta = json!({
            "text": "some non-empty chunk body",
            "episode_id": "01HZ...",
            "heading_path": "# Introduction",
            "offset": 0,
        });
        assert!(validate_chunk_metadata(&meta).is_ok(), "valid metadata must pass");
    }

    #[test]
    fn missing_text_rejected() {
        let meta = json!({
            "episode_id": "01HZ...",
            "heading_path": "# Introduction",
            "offset": 0,
        });
        let err = validate_chunk_metadata(&meta).unwrap_err();
        assert!(
            matches!(err, SchemaError::MissingField { field: "text" }),
            "expected MissingField{{text}}, got: {err}"
        );
    }

    #[test]
    fn empty_text_rejected() {
        let meta = json!({ "text": "" });
        let err = validate_chunk_metadata(&meta).unwrap_err();
        assert!(
            matches!(err, SchemaError::EmptyField { field: "text" }),
            "expected EmptyField{{text}}, got: {err}"
        );
    }

    #[test]
    fn non_string_text_rejected() {
        let meta = json!({ "text": 42 });
        let err = validate_chunk_metadata(&meta).unwrap_err();
        assert!(
            matches!(err, SchemaError::WrongType { field: "text", .. }),
            "expected WrongType{{text}}, got: {err}"
        );
    }

    #[test]
    fn null_text_rejected() {
        let meta = json!({ "text": null });
        let err = validate_chunk_metadata(&meta).unwrap_err();
        assert!(
            matches!(err, SchemaError::WrongType { field: "text", .. }),
            "expected WrongType{{text}}, got: {err}"
        );
    }

    // ── validate_chunk_text ───────────────────────────────────────────────────

    #[test]
    fn validate_chunk_text_accepts_non_empty() {
        assert!(validate_chunk_text("hello world").is_ok());
    }

    #[test]
    fn validate_chunk_text_rejects_empty() {
        let err = validate_chunk_text("").unwrap_err();
        assert!(
            matches!(err, SchemaError::EmptyField { field: "text" }),
            "expected EmptyField{{text}}, got: {err}"
        );
    }

    #[test]
    fn whitespace_only_text_passes() {
        // Policy: we reject bytes-empty only.  Whitespace-only is accepted
        // because the chunker never produces it and rejecting it would be
        // over-eager for any caller using intentional minimal whitespace.
        assert!(validate_chunk_text("   ").is_ok(), "whitespace-only text is NOT rejected by policy");
    }
}
