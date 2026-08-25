//! `memory.repair_vectors` — remove `vec` fields that cannot be KNN candidates.
//!
//! This is a maintenance sweep, not a memory operation. It exists because the
//! F22 write-side guard in `lunaris-storage-moon` is forward-only: it stops new
//! rows reaching the index with an all-zero or non-finite embedding, and does
//! nothing about the rows written before it landed. A survey of the live store
//! found 622 of 1235 chunk rows carrying an all-zero `vec`, and no component in
//! the system removes them — the embed-promotion worker is queue-driven and a
//! legacy row has no event to be woken by.
//!
//! A zero vector is not an absent match, it is a universal one: Moon scores it
//! at distance 1.0 from every query, a flat 0.500, which outranks every genuine
//! hit below cosine 0.5. Because that score is content-independent the symptom
//! is uniform and unattributable — the reason this shipped for a month.
//!
//! ## Why it is here and not in the CLI
//!
//! `lunaris-cli` must never open storage on its own; three recall surfaces
//! diverged that way before GA-1. Putting the sweep behind
//! [`crate::protocol::dispatch`] keeps the CLI a caller like every other
//! surface. It is deliberately NOT exposed as an MCP tool: an agent has no
//! business running a maintenance sweep over its own memory, and no HTTP route
//! offers it either. Reaching it takes the CLI or an explicit dispatch call.
//!
//! ## Why it defaults to a preview
//!
//! `commit` defaults to false, matching `forget`'s CLI surface. An operator
//! pointing this at a production scope is owed the count before the mutation,
//! and a repair that runs by accident is a repair nobody can review.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ServiceError;
use lunaris::Lunaris;
use lunaris_core::{LunarisError, Scope, StorageError};

/// Indices that carry embeddings. `chunks` is the default because it is the
/// only one an operator has ever needed to repair, but the others are reachable
/// by name — they are written by the same `VectorUpsert` path and were exposed
/// to the same pre-guard behaviour.
pub const REPAIRABLE_INDICES: &[&str] = &["chunks", "entities", "facts", "communities"];

fn default_index() -> String {
    "chunks".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepairVectorsParams {
    /// Which vector index to sweep. One of [`REPAIRABLE_INDICES`].
    #[serde(default = "default_index")]
    pub index: String,

    /// Actually remove the fields. Without it the sweep only reports.
    #[serde(default)]
    pub commit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepairVectorsResponse {
    pub index: String,
    /// Rows walked in this scope, damaged or not.
    pub scanned: usize,
    /// Rows whose stored `vec` cannot be a KNN candidate.
    pub unindexable: usize,
    /// Rows actually changed. Always 0 for a preview.
    pub repaired: usize,
    /// True when this was a preview and nothing was written.
    pub dry_run: bool,
}

pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: RepairVectorsParams,
) -> Result<RepairVectorsResponse, ServiceError> {
    if !REPAIRABLE_INDICES.contains(&params.index.as_str()) {
        return Err(ServiceError::InvalidInput(format!(
            "unknown vector index {:?}: expected one of {}. A typo here would \
             report a clean sweep of an index that does not exist, which reads \
             exactly like a store with no damage",
            params.index,
            REPAIRABLE_INDICES.join(", ")
        )));
    }

    // The sweep is Moon-shaped: it walks hash rows by the per-scope FT index
    // key prefix. There is no portable spelling of that, so rather than invent
    // a trait method with one implementation, this asks for the concrete handle
    // and says plainly when it is not there.
    let moon = lunaris.moon_storage().ok_or(ServiceError::LunarisEngine(LunarisError::Storage(
        StorageError::NotSupported(
            "repair_vectors needs a Moon-backed store: it walks Moon hash rows \
             by the per-scope FT index prefix",
        ),
    )))?;

    let report = moon
        .repair_unindexable_vectors(scope, &params.index, !params.commit)
        .await
        .map_err(|e| ServiceError::LunarisEngine(LunarisError::Storage(e)))?;

    Ok(RepairVectorsResponse {
        index: params.index,
        scanned: report.scanned,
        unindexable: report.unindexable,
        repaired: report.repaired,
        dry_run: report.dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_defaults_to_a_preview() {
        let p: RepairVectorsParams = serde_json::from_str("{}").expect("empty params must parse");
        assert!(!p.commit, "omitting `commit` must NOT run a mutating sweep");
        assert_eq!(p.index, "chunks");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // Without `deny_unknown_fields` a caller misspelling `commit` would get
        // a silent preview and believe the store was repaired.
        let e = serde_json::from_str::<RepairVectorsParams>(r#"{"commmit": true}"#);
        assert!(e.is_err(), "a misspelled field must be an error, not a silent default");
    }

    #[test]
    fn every_repairable_index_is_one_lunaris_actually_writes() {
        // Guards against this list drifting from `ensure_indexes`. A name here
        // that Moon never creates would sweep zero rows and report success.
        for idx in REPAIRABLE_INDICES {
            assert!(
                ["chunks", "entities", "facts", "communities"].contains(idx),
                "{idx} is not an index Lunaris creates"
            );
        }
    }
}
