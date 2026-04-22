//! SurfaceIR extractor.
//!
//! Reads the declarative `crates/lunaris-codegen/annotations/surface.toml`
//! file and deserialises it directly into [`SurfaceIR`] via `toml::from_str`.
//! No custom string grammar — structured-table [`IrTyRef`] variants map
//! one-to-one onto TOML tables (see `ir.rs` module docs).
//!
//! The extractor is deterministic: given an unchanged TOML file, the output
//! IR is byte-identical across calls. The
//! `surface_snapshot::extract_is_deterministic` test (in
//! `tests/surface_snapshot.rs`) asserts this explicitly.

use std::path::Path;

use anyhow::{Context, Result};

use crate::ir::SurfaceIR;

/// Path from the `lunaris-codegen` crate root to the committed annotation
/// file. Used by the CLI binary and by the integration tests.
pub const SURFACE_TOML_PATH: &str = "annotations/surface.toml";

/// Extract the IR from the committed annotation file, given the workspace
/// root (typically `env!("CARGO_MANIFEST_DIR")/../..`).
pub fn extract_surface(workspace_root: &Path) -> Result<SurfaceIR> {
    let toml_path = workspace_root.join("crates/lunaris-codegen").join(SURFACE_TOML_PATH);
    let raw = std::fs::read_to_string(&toml_path).with_context(|| {
        format!("lunaris-codegen: failed to read annotation file at {}", toml_path.display())
    })?;
    extract_surface_from_str(&raw).with_context(|| {
        format!("lunaris-codegen: failed to parse annotation file at {}", toml_path.display())
    })
}

/// Parse a TOML string directly. Exposed for unit tests that want to round
/// trip literal TOML without touching the filesystem.
pub fn extract_surface_from_str(raw: &str) -> Result<SurfaceIR> {
    let ir: SurfaceIR =
        toml::from_str(raw).context("lunaris-codegen: TOML deserialisation of SurfaceIR failed")?;
    Ok(ir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrAsync, IrReceiver, IrTyRef};

    fn load_committed_ir() -> SurfaceIR {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let toml_path = Path::new(manifest).join(SURFACE_TOML_PATH);
        let raw = std::fs::read_to_string(&toml_path).expect("read surface.toml");
        extract_surface_from_str(&raw).expect("parse surface.toml")
    }

    #[test]
    fn sixty_one_surface_methods_present() {
        let ir = load_committed_ir();
        // Plan 08-01 surface enumeration (15 methods):
        //
        //   Lunaris: open, ingest, recall, forget, snapshot        (5)
        //   Vector::new                                             (1)
        //   Keyword::bm25                                           (1)
        //   Graph::anchored                                         (1)
        //   RetrievalBuilder: and, fuse_rrf, top, filter, as_of    (5)
        //   GraphPipelineHandle (single toggle)                    (1)
        //   ConsolidatorPipelineHandle (single toggle)             (1)
        //                                                         ----
        //   Phase 8 subtotal:                                       15
        //
        // Plan 11-02b — conversational (25 methods):
        //
        //   ChatAgentMemory: new, remember, recall                 (3)
        //   MultiTurnConversation: new, remember, recall, consolidate (4)
        //   SlackArchive: new, ingest_channel, recall, channel, user (5)
        //   SlackArchiveQuery: with_user, recall                   (2)
        //   EmailThreading: new, ingest, thread, recall,
        //                   with_graph_pipeline                    (5)
        //   MeetingNotesMemory: new, note, recall, attendees,
        //                       with_graph_pipeline                (5)
        //   MeetingNotesQuery: recall                              (1)
        //                                                         ----
        //   conversational subtotal:                                25
        //
        // Plan 11-02b — documentary (21 methods):
        //
        //   DocumentKnowledgeBase: new, ingest, filter, top, search (5)
        //   ResearchPaperCorpus: new, with_graph_pipeline, ingest,
        //                        search                            (4)
        //   CodeRepoMemory: new, ingest_commit, recall             (3)
        //   TimelineReconstruction: new, ingest, between, as_of    (4)
        //   CustomerSupportHistory: new, with_graph_pipeline,
        //                           ingest_ticket, ingest_chat,
        //                           recall                         (5)
        //                                                         ----
        //   documentary subtotal:                                   21
        //                                                         ----
        //   Grand total:                                            61
        let total_methods: usize =
            ir.modules.iter().flat_map(|m| &m.types).map(|t| t.methods.len()).sum();
        assert_eq!(
            total_methods, 61,
            "expected 61 surface methods (15 phase 8 + 25 conversational + 21 documentary), got {total_methods}: {:#?}",
            ir.modules
        );
        // Module count sanity: 3 modules (lunaris + conversational + documentary).
        assert_eq!(ir.modules.len(), 3, "expected 3 modules");
        // Type count sanity: 19 types (7 + 7 + 5).
        let total_types: usize = ir.modules.iter().map(|m| m.types.len()).sum();
        assert_eq!(total_types, 19, "expected 19 types (7 lunaris + 7 conversational + 5 documentary)");
    }

    #[test]
    fn async_methods_are_marked() {
        let ir = load_committed_ir();
        let lunaris = ir
            .modules
            .iter()
            .flat_map(|m| &m.types)
            .find(|t| t.name == "Lunaris")
            .expect("Lunaris type present");
        for async_name in ["open", "ingest", "forget", "snapshot"] {
            let m = lunaris
                .methods
                .iter()
                .find(|m| m.name == async_name)
                .unwrap_or_else(|| panic!("Lunaris::{async_name} present"));
            assert_eq!(m.is_async, IrAsync::Yes, "Lunaris::{async_name} must be async in the IR");
        }
        let recall =
            lunaris.methods.iter().find(|m| m.name == "recall").expect("Lunaris::recall present");
        assert_eq!(
            recall.is_async,
            IrAsync::No,
            "Lunaris::recall is sync (returns RetrievalBuilder)"
        );
    }

    #[test]
    fn extract_is_deterministic() {
        let a = load_committed_ir();
        let b = load_committed_ir();
        let ja = serde_json::to_string_pretty(&a).unwrap();
        let jb = serde_json::to_string_pretty(&b).unwrap();
        assert_eq!(ja, jb, "extract_surface must be deterministic");
    }

    #[test]
    fn snapshot_method_returns_lsn() {
        // BLOCKER 1 resolution: Lunaris::snapshot returns Lsn, not
        // SnapshotId. Plan 08-00 landed the Rust-side implementation with
        // exactly this signature; the IR must match.
        let ir = load_committed_ir();
        let lunaris = ir
            .modules
            .iter()
            .flat_map(|m| &m.types)
            .find(|t| t.name == "Lunaris")
            .expect("Lunaris type present");
        let snap = lunaris
            .methods
            .iter()
            .find(|m| m.name == "snapshot")
            .expect("Lunaris::snapshot present");
        match &snap.returns.ty {
            IrTyRef::Named { name } => assert_eq!(name, "Lsn"),
            other => panic!("expected Named {{ name: \"Lsn\" }}, got {other:?}"),
        }
        assert!(snap.returns.fallible, "snapshot returns Result<Lsn, _>");
        assert_eq!(snap.receiver, IrReceiver::RefSelf);
    }
}
