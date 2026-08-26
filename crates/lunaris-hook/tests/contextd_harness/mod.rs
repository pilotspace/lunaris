//! Shared harness for tests that drive the real `lunaris-contextd` binary.
//!
//! Extracted from `context_hybrid_recall.rs` when `injection_composition.rs`
//! needed the same three helpers. Copying them would have created the shape
//! that bites later: a fix applied to one copy while the other keeps the bug.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

/// Spawn the real `lunaris-contextd` binary on a temp socket with the given
/// extra env; wait until the socket accepts connections.
pub async fn spawn_contextd(socket: &Path, extra_env: &HashMap<&str, String>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lunaris-contextd"));
    cmd.arg("--socket")
        .arg(socket)
        .env_remove("LUNARIS_CONTEXT_RECALL")
        .env_remove("LUNARIS_CONTEXT_RECALL_TIMEOUT_MS")
        .env_remove("LUNARIS_STORE_URL")
        // W4.4 — the demotion toggle must never leak in from the developer's
        // shell: an operator with it set would turn the ratchet green by
        // re-admitting exactly the telemetry the test exists to exclude.
        .env_remove("LUNARIS_CONTEXT_INCLUDE_TOOLCALLS")
        .env_remove("LUNARIS_CONTEXT_PROMPT_INCLUDE_TOOLCALLS")
        .kill_on_drop(true);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn lunaris-contextd");

    for _ in 0..200 {
        if UnixStream::connect(socket).await.is_ok() {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("lunaris-contextd never bound {}", socket.display());
}

/// One request/response round-trip over the contextd socket protocol
/// (write JSON, shutdown write half, read to EOF).
pub async fn request(socket: &Path, body: &Value) -> Value {
    let round_trip = async {
        let mut stream = UnixStream::connect(socket).await.expect("connect contextd socket");
        stream.write_all(body.to_string().as_bytes()).await.expect("write request");
        stream.shutdown().await.expect("shutdown write half");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read response");
        serde_json::from_slice::<Value>(&buf).expect("contextd responds with JSON")
    };
    // Generous budget: the FIRST request lazily opens the Lunaris handle and
    // loads the GGUF embedder inside the contextd process.
    tokio::time::timeout(Duration::from_secs(180), round_trip)
        .await
        .expect("contextd answered within budget")
}

/// Env for a routing-only contextd: real store, deliberately absent GGUFs so
/// the fast `NoopEmbedder` fallback is taken. Recall still works — hybrid's
/// keyword leg is text-based — and CI pays for no model download.
pub fn memory_env(store_url: &str) -> HashMap<&'static str, String> {
    let mut env = HashMap::new();
    env.insert("LUNARIS_STORE_URL", store_url.to_owned());
    env.insert("LUNARIS_EMBEDDER_GGUF", "/nonexistent/embedder.gguf".to_owned());
    env.insert("LUNARIS_RERANKER_GGUF", "/nonexistent/reranker.gguf".to_owned());
    env
}
