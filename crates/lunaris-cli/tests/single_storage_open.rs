//! Structural ratchet: this crate may open storage in exactly ONE place.
//!
//! The value of a fourth surface is entirely conditional on it asking the SAME
//! question as the other three. GA-1 (PR #126) had to unify three recall
//! pipelines that had quietly drifted apart — MCP dropped fact legs under
//! `with_root`, the hook ran `hybrid_root` fact legs un-gated, HTTP/SDK was
//! vector-only — and every one of those diverged because the surface held its
//! own handle and therefore planned its own retrieval.
//!
//! So: `src/direct.rs` opens a handle, hands it to
//! `lunaris_memory_service::protocol::dispatch`, and that is the only place in
//! the crate allowed to do so. Anywhere else is the beginning of a fourth
//! divergence, and it would not look like a bug in review — it would look like
//! a reasonable shortcut.
//!
//! This lives in `tests/` rather than beside the code deliberately. The first
//! draft sat in `src/request.rs` and failed on its own source: a scanner that
//! walks the directory it lives in matches the pattern literal in its own
//! body. Keeping it outside `src/` removes that whole class of problem, and
//! the needle is still assembled at runtime so the file cannot self-match if
//! anyone moves it back.

use std::path::{Path, PathBuf};

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The only module permitted to hold a `Lunaris` handle.
const FALLBACK_MODULE: &str = "direct.rs";

#[test]
fn storage_is_opened_only_in_the_direct_fallback() {
    // Assembled, never written literally — see the module docs.
    let needles = [format!("Lunaris{}open", "::"), format!("lunaris{}open(", "::")];

    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(src_dir()).expect("read src/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == FALLBACK_MODULE {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("read source");
        for (i, line) in body.lines().enumerate() {
            // Comments may name the symbol freely; prose is not a call site.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if needles.iter().any(|n| line.contains(n.as_str())) {
                offenders.push(format!("{name}:{}", i + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "storage is opened outside {FALLBACK_MODULE} at {offenders:?}. Every \
         subcommand must reach the store through \
         lunaris_memory_service::protocol::dispatch — the same function \
         lunaris-contextd runs and the lunaris-mcp proxy falls back to. \
         Holding a handle here lets this surface plan its own retrieval, which \
         is exactly how the three pre-GA-1 recall pipelines diverged."
    );
}

/// `lunaris try` needed a handle of its own — it drives six ingests and a
/// recall against a store it just started, so a per-request open would reload
/// the model seven times. The tempting shortcut was to let `trial.rs` call the
/// constructor directly; the ratchet above already forbids that, and this test
/// records the positive half: `try` reaches storage through `direct`, on the
/// same `dispatch` every other subcommand uses.
///
/// Without this, deleting `trial.rs`'s use of `direct::` and inlining a private
/// pipeline would leave the scanner above perfectly green.
#[test]
fn the_trial_reaches_storage_through_the_direct_module() {
    let body = std::fs::read_to_string(src_dir().join("trial.rs")).expect("read trial.rs");
    let code = || {
        body.lines().filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with('*')
        })
    };

    assert!(
        code().any(|l| l.contains("direct::open_handle(")),
        "trial.rs no longer opens its store through crate::direct. Every surface \
         that holds its own handle plans its own retrieval, which is exactly how \
         the three pre-GA-1 recall pipelines diverged — a trial command that \
         demonstrates a DIFFERENT recall than the product is worse than no trial."
    );
    assert!(
        code().any(|l| l.contains("direct::dispatch_on(")),
        "trial.rs no longer runs its requests through the shared dispatch"
    );
    assert!(
        !code().any(|l| l.contains("lunaris_memory_service::protocol::dispatch")),
        "trial.rs calls dispatch directly instead of going through crate::direct. \
         Keep one seam: direct.rs owns the handle AND the dispatch call, so there \
         is exactly one place to look when the surfaces are suspected of drifting."
    );
}

/// The fallback must still exist. If `direct.rs` ever stops opening a handle,
/// either the crate lost its offline path or the open moved somewhere the test
/// above now has to police — both worth failing on rather than discovering
/// later.
#[test]
fn the_direct_fallback_actually_opens_storage() {
    let body =
        std::fs::read_to_string(src_dir().join(FALLBACK_MODULE)).expect("read the fallback module");
    let needle = format!("Lunaris{}open", "::");
    assert!(
        body.lines().any(|l| !l.trim_start().starts_with("//") && l.contains(&needle)),
        "{FALLBACK_MODULE} no longer opens a handle. If the direct path was \
         removed on purpose, delete both tests deliberately; if the open just \
         moved, the sibling test above is now policing the wrong file."
    );
}
