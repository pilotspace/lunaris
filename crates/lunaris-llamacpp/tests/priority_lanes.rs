//! nonblocking-embedder-arch — the real production embed paths route through
//! the priority-aware worker without changing embeddings or the one-context
//! footprint contract.
//!
//! Scope split (honest): the DETERMINISTIC preemption ordering (Interactive
//! drains before Background) is pinned model-free by
//! `worker::intake_tests` — no GGUF, no timing race. THIS test is the
//! built≠wired proof: it drives the real `LlamaCppEmbedder` through BOTH
//! lanes and asserts (a) priority never changes the embedding (byte-identical
//! Interactive vs Background), and (b) concurrent mixed-lane traffic still
//! shares exactly ONE llama.cpp context (the footprint contract we just tuned).
//! Model-gated: skips when the granite GGUF is not staged.

#![cfg(feature = "llamacpp")]

use std::path::PathBuf;

use lunaris_core::Embedder;
use lunaris_llamacpp::{LlamaCppEmbedder, LlamaCppEmbedderOpts};

fn embedder_gguf() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUNARIS_EMBEDDER_GGUF").map(PathBuf::from) {
        return Some(p);
    }
    std::env::var_os("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join(".lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
        })
        .filter(|p| p.exists())
}

#[tokio::test]
async fn both_lanes_share_one_context_and_agree() {
    let Some(gguf_path) = embedder_gguf() else {
        eprintln!("[skip] both_lanes_share_one_context_and_agree — granite GGUF not staged");
        return;
    };
    let embedder = LlamaCppEmbedder::open(LlamaCppEmbedderOpts {
        gguf_path,
        n_gpu_layers: if cfg!(feature = "metal") { u32::MAX } else { 0 },
        ..Default::default()
    })
    .expect("open");

    let inputs = ["a tiny memo", "another memo"];
    // Interactive (embed_batch) and Background (embed_batch_lowpri) must be
    // byte-identical: priority changes SCHEDULING, never the embedding value
    // or the input ordering.
    let hi = embedder.embed_batch(&inputs).await.expect("interactive embed");
    let lo = embedder.embed_batch_lowpri(&inputs).await.expect("background embed");
    assert_eq!(hi, lo, "lane must not change embeddings — priority is scheduling only");
    assert_eq!(hi[0].len(), 768);

    // Concurrent mixed-lane submission: a background batch in flight while an
    // interactive query arrives. Both complete correctly against ONE context.
    let bg_embedder = embedder.clone();
    let bg = tokio::spawn(async move {
        bg_embedder.embed_batch_lowpri(&["bg one", "bg two", "bg three"]).await
    });
    let fg = embedder.embed_batch(&["fg query"]).await.expect("interactive query");
    let bg_rows = bg.await.expect("bg join").expect("background embed");

    assert_eq!(fg.len(), 1);
    assert_eq!(fg[0].len(), 768);
    assert_eq!(bg_rows.len(), 3);
    assert_eq!(
        embedder.contexts_created(),
        1,
        "priority lanes must share ONE warm context — a second context breaks the footprint contract"
    );
}
