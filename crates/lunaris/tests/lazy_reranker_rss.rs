//! N-04 D1 — lazy quantized-reranker load gate.
//!
//! Asserts that constructing a `Lunaris` handle with
//! `LUNARIS_RERANKER_GGUF` pointing at a real 446 MiB Q5_K_M-imatrix
//! GGUF does **NOT** materialize the ~446 MiB resident-set bump until
//! `rerank()` is first called. The pre-N-04 implementation
//! eagerly opened the GGUF inside `resolve_reranker()` during
//! `Lunaris::open()` — boosting RSS by hundreds of MiB on every handle
//! creation, even when the recall path never reached the rerank stage.
//!
//! ## Portability
//!
//! The test is `#[ignore]`d by default. CI / operators set
//! `LUNARIS_RERANKER_GGUF` + `LUNARIS_RERANKER_DIR` to the staged
//! artifacts and run `cargo test -p lunaris-memory --test
//! lazy_reranker_rss -- --ignored`. The build profile MUST be `release`
//! to match measured baselines (debug allocator overhead inflates RSS).
//!
//! On Windows / unsupported targets the test prints a SKIP message and
//! returns.
//!
//! ## Threshold
//!
//! Allowed delta: 100 MiB. The empty-handle baseline (FP32 reranker
//! absent + NoopReranker fallback on cache miss is OK, this test only
//! cares about the GGUF mmap) is on the order of ~50 MiB.
//! Eagerly-loaded GGUF crosses 400 MiB delta. The 100 MiB ceiling
//! catches eager loading without flaking on allocator high-water marks.

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::{Embedder, NoopEmbedder};
use lunaris_rerank::RerankCandidate;
use lunaris_test_harness::open_test_store;

const RSS_TOLERANCE_BYTES: u64 = 100 * 1024 * 1024; // 100 MiB

#[tokio::test]
#[ignore = "requires LUNARIS_RERANKER_GGUF + LUNARIS_RERANKER_DIR pointed at real artifacts; run with --ignored under --release"]
async fn open_does_not_materialize_gguf_into_rss() {
    if !rss_supported() {
        eprintln!("SKIP: RSS sampling not supported on this target");
        return;
    }
    let Some(gguf) = std::env::var("LUNARIS_RERANKER_GGUF").ok().filter(|s| !s.is_empty()) else {
        eprintln!("SKIP: LUNARIS_RERANKER_GGUF not set");
        return;
    };
    if !std::path::Path::new(&gguf).exists() {
        eprintln!("SKIP: LUNARIS_RERANKER_GGUF points at non-existent file: {gguf}");
        return;
    }

    // 0.7.0 port off `memory://`: the store is resolved BEFORE the baseline
    // sample so the harness's own allocations (and the Moon child's spawn) sit
    // under `rss_before` rather than inside the 100 MiB open() budget. The Moon
    // itself is a separate process and contributes no RSS to this one.
    let store = open_test_store().await;

    let rss_before = sample_rss().expect("RSS sample (before open)");

    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let handle = Lunaris::open_with_embedder(store.url(), embedder)
        .await
        .expect("open_with_embedder must succeed");

    let rss_after_open = sample_rss().expect("RSS sample (after open)");
    let delta_open = rss_after_open.saturating_sub(rss_before);

    eprintln!(
        "RSS before={rss_before} after_open={rss_after_open} delta={delta_open} \
         (tolerance={RSS_TOLERANCE_BYTES})"
    );

    assert!(
        delta_open <= RSS_TOLERANCE_BYTES,
        "open() must NOT mmap the {gguf} GGUF eagerly — RSS delta {delta_open} > {RSS_TOLERANCE_BYTES} (~100 MiB)"
    );

    // Now trigger first rerank — this is when the load IS allowed.
    let docs = vec![RerankCandidate {
        id: b"doc-1".to_vec(),
        text: "hello world".to_string(),
        score: 0.0,
        metadata: serde_json::Value::Null,
    }];
    let _ = handle.reranker().rerank("hi", docs).await;
    // We don't assert RSS after rerank — the test's contract is "open is
    // cheap", not "rerank stays cheap".
}

// ---------------------------------------------------------------------------
// RSS sampler — mirrors lunaris-bench::eval::memory::sample_rss but inlined
// here to keep the test free of bench-only deps and avoid pulling procfs into
// the lunaris-memory crate's test build.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn sample_rss() -> Option<u64> {
    use std::fs;
    let stat = fs::read_to_string("/proc/self/statm").ok()?;
    let mut it = stat.split_whitespace();
    let _size = it.next()?;
    let rss_pages: u64 = it.next()?.parse().ok()?;
    Some(rss_pages * 4096)
}

#[cfg(target_os = "macos")]
fn sample_rss() -> Option<u64> {
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    let kb: u64 = stdout.trim().parse().ok()?;
    Some(kb * 1024)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sample_rss() -> Option<u64> {
    None
}

const fn rss_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}
