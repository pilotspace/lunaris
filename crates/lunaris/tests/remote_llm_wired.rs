//! Phase C2 acceptance (llama.cpp-only cutover, decision 1) — extraction is
//! REMOTE-ONLY, and the production `Lunaris::open()` path constructs the
//! remote extractor from provider env vars.
//!
//! Zero-config = degraded mode (NoopExtractor: episodes/chunks/recall work,
//! no graph/facts) — covered by every existing default-open test. THIS test
//! pins the other half: `LUNARIS_EXTRACT_PROVIDER=openai-compat` (+ base
//! URL + model envs) must make `open()` wire a real `CloudApiExtractor`
//! WITHOUT any `with_extractor` call — built ≠ wired, so the discriminator
//! is behavioral: a fake local `/v1/chat/completions` server counts the
//! requests that arrive while `ingest()` runs. The pre-cutover resolver
//! (candle-or-Noop) sends ZERO requests under this env.

#![cfg(feature = "cloud-api")]
// `std::env::set_var` is unsafe in Rust 2024; permitted at the test-binary
// level only (mirrors `default_flip.rs` / `llamacpp_wired.rs`). Single-test
// binary, env writes happen before any `.await`.
#![allow(unsafe_code)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lunaris::{Episode, Lunaris};
use lunaris_test_harness::open_test_store;

/// Minimal OpenAI-compatible server: serves every connection a canned
/// chat-completions body whose content is an empty-but-valid extraction,
/// counting requests. Runs until the listener is dropped at process exit.
fn spawn_counting_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
    let port = listener.local_addr().expect("local addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_in_thread = Arc::clone(&hits);
    let content = r#"{\"entities\":[],\"relations\":[]}"#;
    let body = format!(r#"{{"choices":[{{"message":{{"content":"{content}"}}}}]}}"#);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = body.clone();
            let hits = Arc::clone(&hits_in_thread);
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let Ok(n) = stream.read(&mut tmp) else { return };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                        let content_length = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim())
                            })
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() - (pos + 4) >= content_length {
                            break;
                        }
                    }
                    if n == 0 {
                        return;
                    }
                }
                hits.fetch_add(1, Ordering::SeqCst);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });
    (port, hits)
}

#[tokio::test]
async fn env_configured_openai_compat_extractor_serves_the_ingest_path() {
    let (port, hits) = spawn_counting_server();

    // SAFETY-NOTE: single-test binary, set before any `.await`.
    unsafe {
        std::env::set_var("LUNARIS_EXTRACT_PROVIDER", "openai-compat");
        std::env::set_var("LUNARIS_OPENAI_COMPAT_BASE_URL", format!("http://127.0.0.1:{port}/v1"));
        std::env::set_var("OPENAI_COMPAT_EXTRACT_MODEL", "fake-extract-model");
        // The extractor branch in `ingest()` is gated on the graph pipeline
        // (D-10, default OFF) — enable it or the wired extractor is never
        // invoked and this test can't discriminate anything.
        std::env::set_var("LUNARIS_GRAPH_ENABLED", "1");
    }

    // 0.7.0 port off `memory://`. `Lunaris::open` (not the harness's
    // `open_test_engine`) because this test discriminates on the RESOLVED
    // extractor/embedder, which a harness-supplied StubEmbedder would mask.
    let store = open_test_store().await;
    let handle = Lunaris::open(store.url()).await.expect("open test store");
    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "remote-llm-wired.md",
        "Alice Smith moved to Paris in 2019 and works at Acme Corp as a staff engineer.",
        &handle.clock(),
    );
    handle.ingest(ep).await.expect("ingest through the production pipeline");

    let n = hits.load(Ordering::SeqCst);
    assert!(
        n > 0,
        "LUNARIS_EXTRACT_PROVIDER=openai-compat must wire CloudApiExtractor into the \
         production ingest path — the fake /v1/chat/completions server received {n} requests \
         (0 means the resolver ignored the provider env and used Noop/candle)"
    );
}
