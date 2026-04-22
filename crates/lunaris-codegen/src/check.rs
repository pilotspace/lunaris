//! Parity-check walker.
//!
//! BLOCKER 3+4 resolution (Plan 08-01 `<threat_model>` T-08-01-05): the
//! walker enumerates an EXPLICIT list of codegen-managed snapshot paths
//! rather than recursively scanning binding source directories. This keeps
//! handwritten `conformance.rs` modules (owned by Plans 08-02 / 08-03 /
//! 08-04 under the `bindings-it` feature) invisible to parity-check by
//! construction — a future refactor turning this into a recursive walker
//! MUST preserve the `conformance.rs` carveout.
//!
//! The three managed paths live inside the `lunaris-codegen` crate itself
//! because the Wave 2 / Wave 3 host crates (`lunaris-py`, `lunaris-ts`) do
//! not exist yet. Plans 08-02 / 08-03 will later add a second walker pass
//! for the real `crates/lunaris-{py,ts}/src/generated.rs` files; that
//! additive change will adjust [`codegen_managed_paths`] without touching
//! the conformance-exclusion contract.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use similar::TextDiff;

use crate::emit_py::emit_py;
use crate::emit_ts::emit_ts;
use crate::ir::SurfaceIR;

/// One codegen-managed file path + its regenerated expected content.
#[derive(Debug, Clone)]
pub struct ManagedPath {
    /// Path relative to the workspace root.
    pub relative: PathBuf,
    /// Regenerated content the committed file MUST match (modulo trailing
    /// whitespace).
    pub expected: String,
}

/// Outcome of a `--check` run.
#[derive(Debug, Default)]
pub struct CheckOutcome {
    pub drift: Vec<DriftEntry>,
}

#[derive(Debug)]
pub struct DriftEntry {
    pub path: PathBuf,
    pub diff: String,
}

impl CheckOutcome {
    pub fn is_clean(&self) -> bool {
        self.drift.is_empty()
    }
}

/// Return the explicit list of codegen-managed paths + their regenerated
/// content for the given IR. The list is intentionally NOT a recursive
/// directory scan — see module docs for the conformance-exclusion
/// rationale.
pub fn codegen_managed_paths(ir: &SurfaceIR) -> Vec<ManagedPath> {
    let py = emit_py(ir);
    let ts = emit_ts(ir);
    vec![
        ManagedPath {
            relative: PathBuf::from("crates/lunaris-codegen/snapshots/generated_py.rs"),
            expected: py,
        },
        ManagedPath {
            relative: PathBuf::from("crates/lunaris-codegen/snapshots/generated_ts.rs"),
            expected: ts.rust_glue,
        },
        ManagedPath {
            relative: PathBuf::from("crates/lunaris-codegen/snapshots/generated_ts.d.ts"),
            expected: ts.dts,
        },
    ]
}

/// Diff the regenerated IR against the committed snapshot files. Returns
/// `CheckOutcome` with one entry per drifting path (empty on a clean tree).
pub fn check_surface(workspace_root: &Path, ir: &SurfaceIR) -> Result<CheckOutcome> {
    let mut outcome = CheckOutcome::default();
    for managed in codegen_managed_paths(ir) {
        let abs = workspace_root.join(&managed.relative);
        let committed = std::fs::read_to_string(&abs).unwrap_or_default();
        if committed.trim() != managed.expected.trim() {
            let diff = TextDiff::from_lines(&committed, &managed.expected)
                .unified_diff()
                .header(
                    &format!("committed {}", managed.relative.display()),
                    &format!("regenerated {}", managed.relative.display()),
                )
                .to_string();
            outcome.drift.push(DriftEntry {
                path: managed.relative.clone(),
                diff,
            });
        }
    }
    // Sanity: the walker enumerated exactly three codegen-managed paths,
    // matching the list committed to `snapshots/`. Anything outside this
    // list — including any `conformance.rs` under `crates/lunaris-py/src/`
    // or `crates/lunaris-ts/src/` once those directories exist — is NOT
    // compared. Test `check_mode_skips_conformance_rs` in
    // `tests/emitter_shape.rs` locks the contract.
    let _ = workspace_root; // silence unused when drift check returns empty
    Ok(outcome)
}

/// Write the regenerated snapshots to disk. Used by the `--emit` CLI flag.
pub fn write_snapshots(workspace_root: &Path, ir: &SurfaceIR) -> Result<()> {
    for managed in codegen_managed_paths(ir) {
        let abs = workspace_root.join(&managed.relative);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("lunaris-codegen: create_dir_all {}", parent.display())
            })?;
        }
        std::fs::write(&abs, &managed.expected)
            .with_context(|| format!("lunaris-codegen: write {}", abs.display()))?;
    }
    Ok(())
}
