//! One catalogue, one copy — the W0.7 guard.
//!
//! Before W0.7 the workspace carried the GGUF mirror URL and its pinned
//! SHA-256 in five places: two full staging implementations
//! (`lunaris-mcp/src/model_stager.rs` and `lunaris-cli/src/stage.rs`) and
//! three CI workflows, held together by comments asking the next person to
//! keep them in sync. A duplicated integrity pin is only safe while something
//! proves the copies agree, and prose does not prove anything: the day one
//! copy re-pins the mirror, the MCP server and `lunaris try` stage different
//! weights under the same filename and every comparison between them stops
//! meaning anything.
//!
//! These tests key on the **decision** — "there is exactly one place that
//! names the artifact" — not on a phrasing. A sixth copy written in a style
//! nobody anticipated still trips the first test, because a copy that does
//! not carry the digest cannot download the model.

use std::path::{Path, PathBuf};

use lunaris_core::models::ModelKind;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Every `.rs` file under `crates/*/src/`, recursively.
fn workspace_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    let crates = repo_root().join("crates");
    for e in std::fs::read_dir(&crates).expect("crates/ must be readable").flatten() {
        let src = e.path().join("src");
        if src.is_dir() {
            walk(&src, &mut out);
        }
    }
    assert!(out.len() > 100, "the source walk found only {} files — the walk is broken", out.len());
    out
}

#[test]
fn exactly_one_source_file_names_the_pinned_artifact() {
    for kind in [ModelKind::EmbedderGraniteQ4KM, ModelKind::RerankerBgeV2M3Q5KM] {
        let digest = kind.sha256();
        let carriers: Vec<_> = workspace_sources()
            .into_iter()
            .filter(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains(digest)))
            .collect();

        assert_eq!(
            carriers.len(),
            1,
            "{} files name the {digest} digest, expected exactly 1 (the catalogue): {:#?}",
            carriers.len(),
            carriers
        );
        assert!(
            carriers[0].ends_with("lunaris-core/src/models.rs"),
            "the pinned digest lives in {} — it belongs in lunaris-core/src/models.rs, \
             the one module every stager reads it from",
            carriers[0].display()
        );
    }
}

/// The CI workflows cannot call Rust to learn the digest, so they carry
/// literals. That is legitimate — but the literals must be *this* catalogue's,
/// and nothing but this test checks that.
#[test]
fn every_workflow_pins_the_catalogue_url_and_digest() {
    let wf_dir = repo_root().join(".github").join("workflows");
    let mut checked = 0usize;

    for e in std::fs::read_dir(&wf_dir).expect(".github/workflows must be readable").flatten() {
        let p = e.path();
        if !p.extension().is_some_and(|x| x == "yml" || x == "yaml") {
            continue;
        }
        let src = std::fs::read_to_string(&p).expect("workflow must be readable");
        let name = p.file_name().unwrap().to_string_lossy().to_string();

        for line in src.lines() {
            let line = line.trim();
            // `KEY: value` env entries only; comments carry the same strings
            // as prose and are not what CI actually downloads.
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else { continue };
            let value = value.trim();
            let expected = match key.trim() {
                "EMBEDDER_GGUF_URL" => ModelKind::EmbedderGraniteQ4KM.url(),
                "EMBEDDER_GGUF_SHA256" => ModelKind::EmbedderGraniteQ4KM.sha256(),
                "RERANKER_GGUF_URL" => ModelKind::RerankerBgeV2M3Q5KM.url(),
                "RERANKER_GGUF_SHA256" => ModelKind::RerankerBgeV2M3Q5KM.sha256(),
                _ => continue,
            };
            checked += 1;
            assert_eq!(
                value,
                expected,
                "{name} pins a {} that the lunaris-core catalogue does not: CI would verify \
                 bytes the engine never asked for",
                key.trim()
            );
        }
    }

    assert!(
        checked >= 4,
        "found only {checked} GGUF env pins across the workflows — either the workflows stopped \
         staging models or this scan stopped matching them; both make the check vacuous"
    );
}

/// The staged path is where the engine looks. Staging 253 MB under a name or
/// a directory `Lunaris::open` does not consult is a silent no-op that
/// presents as success — the failure mode this whole catalogue exists to
/// close.
#[test]
fn the_staged_path_is_home_lunaris_models_plus_the_filename() {
    let home = Path::new("/tmp/some-home");
    assert_eq!(
        lunaris_core::models::staged_path_in(home, ModelKind::EmbedderGraniteQ4KM),
        home.join(".lunaris")
            .join("models")
            .join("granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
    );
    assert_eq!(
        lunaris_core::models::staged_path_in(home, ModelKind::RerankerBgeV2M3Q5KM),
        home.join(".lunaris").join("models").join("bge-reranker-v2-m3.Q5_K_M.gguf")
    );
}
