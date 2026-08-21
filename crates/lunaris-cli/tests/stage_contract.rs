//! The two model stagers must name the same artifact.
//!
//! `lunaris-cli/src/stage.rs` is a deliberate second copy of
//! `lunaris-mcp/src/model_stager.rs`, because the original is `pub(crate)` and
//! `lunaris try` cannot wait for W0.7 to promote it into `lunaris-core`. A
//! duplicated constant is only safe while something proves the copies agree —
//! otherwise the day somebody re-pins the mirror in one file, the MCP server
//! and the CLI quietly stage different weights and every comparison between
//! them stops meaning anything.
//!
//! When W0.7 lands, delete both copies and this file with them.

use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The mcp source writes its URL across two lines as a `\`-continued string
/// literal. The compiler eats the backslash and the following indentation, but
/// we are reading SOURCE text, where both are still there — so strip whitespace
/// and line continuations before comparing.
fn squeeze(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace() && *c != '\\').collect()
}

#[test]
fn both_stagers_pin_the_same_embedder_sha256() {
    let cli = read("crates/lunaris-cli/src/stage.rs");
    let mcp = read("crates/lunaris-mcp/src/model_stager.rs");

    // The digest the CLI pins, lifted out of its own source so the test cannot
    // drift from the constant it is guarding.
    const SHA: &str = "58d27f63e69ccf7abce27bf6b35bb0edebc3a1c05ad4a3165acaba1cdca107c0";
    assert!(cli.contains(SHA), "lunaris-cli/src/stage.rs no longer pins {SHA}");
    assert!(
        mcp.contains(SHA),
        "lunaris-mcp/src/model_stager.rs pins a different embedder digest than \
         lunaris-cli/src/stage.rs. The two stagers must fetch the same weights or \
         `lunaris try` and the MCP server are running different models under the \
         same name."
    );
}

#[test]
fn both_stagers_use_the_same_mirror_and_filename() {
    let cli = squeeze(&read("crates/lunaris-cli/src/stage.rs"));
    let mcp = squeeze(&read("crates/lunaris-mcp/src/model_stager.rs"));

    let url_tail = squeeze(
        "mykor/granite-embedding-311m-multilingual-r2-GGUF/resolve/main/\
         granite-embedding-311M-multilingual-r2-Q4_K_M.gguf",
    );
    assert!(cli.contains(&url_tail), "the CLI stager no longer points at the pinned mirror");
    assert!(mcp.contains(&url_tail), "the mcp stager moved mirrors; the CLI copy did not");

    // The staged FILENAME is what `lunaris::handle::llamacpp_gguf_path` looks
    // for. Staging 253 MB under a name the engine ignores is a silent no-op
    // that presents as success.
    let staged = squeeze("granite-embedding-311m-multilingual-r2.Q4_K_M.gguf");
    assert!(cli.contains(&staged));
    assert!(mcp.contains(&staged));

    let engine = squeeze(&read("crates/lunaris/src/handle.rs"));
    assert!(
        engine.contains(&staged),
        "the engine no longer looks for {staged} under ~/.lunaris/models — both \
         stagers are now writing to a path nothing reads"
    );
}
