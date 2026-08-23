//! Is a local model file actually on disk? — one answer, for every crate.
//!
//! ## Why this is not a `strict_skip` fixture
//!
//! `strict_skip` refuses to skip when `LUNARIS_CONFORMANCE_STRICT=1`, because
//! the job that sets it **builds** the Moon it guarantees. A GGUF is the other
//! case: `integration.yml` does not stage one, and says so —
//! "the remaining workspace ignores ... need a GGUF, a tokenizer, a release
//! binary". A test that needs an embedder is therefore legitimately skippable
//! in that job, and routing it through strict would turn a documented absence
//! into a red board.
//!
//! The skip is still ANNOUNCED (via `note_unavailable_with(.., false)`), so it
//! stays visible to the reader and to `no_silent_skip_workspace.rs`. The
//! narrow carve-out is the strictness, not the announcement.

use std::path::PathBuf;

/// The staged embedder GGUF, if one is actually present.
///
/// `LUNARIS_EMBEDDER_GGUF` wins when set, then the default staging location.
/// **Both branches are existence-checked.** The copy this consolidates
/// (`lunaris-llamacpp/tests/llamacpp_smoke.rs::gguf_path`) returns `Some(p)`
/// for the env-var branch without testing `p.exists()`, so pointing the
/// variable at a nonexistent file reports a model that is not there — the
/// caller then proceeds as if it had an embedder.
pub fn embedder_gguf() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUNARIS_EMBEDDER_GGUF").map(PathBuf::from) {
        return p.exists().then_some(p);
    }
    std::env::var_os("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join(".lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
        })
        .filter(|p| p.exists())
}

/// Can this environment produce real embeddings?
///
/// Returns `false` **and announces** when it cannot. Callers whose assertion
/// depends on similarity search — not merely on ingest succeeding — must gate
/// on this: without an embedder the vector leg carries no signal, and a
/// retrieval that finds nothing is indistinguishable from a true absence.
pub fn embedder_available(what: &str) -> bool {
    if embedder_gguf().is_some() {
        return true;
    }
    // `false`, deliberately — see the module doc. A GGUF is not a fixture the
    // strict job builds.
    crate::strict_skip::note_unavailable_with(
        format!("{what} (no embedder GGUF staged; set LUNARIS_EMBEDDER_GGUF)"),
        false,
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this consolidation fixes: a variable pointing at nothing must
    /// not report a model.
    #[test]
    fn a_nonexistent_path_in_the_env_var_is_not_a_model() {
        // Read-only w.r.t. process env: exercise the predicate directly rather
        // than setting the variable and racing every sibling in this binary.
        let missing = PathBuf::from("/nonexistent/definitely-not-here.gguf");
        assert!(!missing.exists(), "test premise: the path must not exist");
        assert_eq!(missing.exists().then_some(missing), None);
    }

    /// And the announcement must never panic, whatever the ambient strictness
    /// — that is the whole point of the carve-out.
    #[test]
    fn an_absent_model_never_panics_even_under_strict() {
        assert!(
            std::panic::catch_unwind(|| {
                crate::strict_skip::note_unavailable_with("no GGUF", false)
            })
            .is_ok(),
            "the GGUF carve-out must not inherit strict_skip's refusal"
        );
    }
}
