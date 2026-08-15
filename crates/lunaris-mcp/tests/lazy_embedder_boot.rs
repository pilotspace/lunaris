//! Integration guard: the MCP server boots WITHOUT loading embedder weights,
//! and a Direct-route embed op fails honestly when no backend is available.
//!
//! Unified-inference contract (2026-07-19 CPU optimization): lunaris-contextd
//! is the single resident inference host on a developer machine. lunaris-mcp
//! must NOT load a resident GGUF at bootstrap — it proxies embed ops to
//! contextd, and only lazily loads local weights when it truly has to serve
//! an embed op Direct (the npx/uvx standalone story, where no contextd
//! exists). Two behaviors are pinned here, against the REAL binary:
//!
//! 1. Boot succeeds with NO usable embedder configured and NO probe-skip env.
//!    (Before the lazy cutover, `AppState::bootstrap` eagerly resolved the
//!    embedder and refused to start: `NoEmbedderWeights`.)
//! 2. `memory.recall` served Direct with no backend available returns an
//!    actionable error naming the embedder — never a silent empty hit list
//!    (the `mcp-recall-empty-hits` bug class the old boot probe guarded).

use std::process::Stdio;
use std::time::Duration;

use lunaris_test_harness::open_test_store;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn boots_without_weights_and_recall_errors_honestly() {
    let bin = env!("CARGO_BIN_EXE_lunaris-mcp");
    // 0.7.0 port off `memory://`. Bound for the child's whole lifetime — it
    // owns the Moon.
    let store = open_test_store().await;

    let mut child = Command::new(bin)
        .arg("--storage")
        .arg(store.url())
        // Force the resolve chain to have NO usable backend: the GGUF path is
        // nonexistent, staging is skipped, and contextd is disabled so the
        // proxy must serve Direct. Deliberately NO LUNARIS_MCP_SKIP_EMBEDDER_PROBE.
        .env("LUNARIS_EMBEDDER_GGUF", "/nonexistent/lazy-boot-test.gguf")
        .env("LUNARIS_MCP_SKIP_STAGE", "1")
        .env("LUNARIS_MCP_DISABLE_CONTEXTD", "1")
        .env("LUNARIS_MCP_SCOPE", "ci-lazy-boot")
        .env("LUNARIS_MCP_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lunaris-mcp binary");

    let mut stdin = child.stdin.take().expect("child stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("child stdout")).lines();

    let handshake = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"ci","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory.ingest","arguments":{"source":"test/lazy","content":"Ingest must work without a dense embedder — KV + BM25 need no vectors."}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory.recall","arguments":{"query":"lazy embedder probe","k":3}}}"#,
        "\n",
    );
    stdin.write_all(handshake.as_bytes()).await.expect("write handshake");
    stdin.flush().await.expect("flush handshake");

    // Behavior 1: the server must answer tools/list — i.e. bootstrap did NOT
    // eagerly resolve + probe the embedder and die on missing weights.
    let mut tools_line = None;
    let mut ingest_line = None;
    let mut recall_line = None;
    // rmcp dispatches the two tool calls CONCURRENTLY, so id:3 (ingest) and
    // id:4 (recall) can arrive in either order — the fast Noop recall probe
    // often replies before the slower SQLite ingest. Collect until we have
    // BOTH tool replies (plus tools/list); never break on one id alone.
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await.expect("read stdout line") {
            if line.contains("\"id\":2") {
                tools_line = Some(line);
            } else if line.contains("\"id\":3") {
                ingest_line = Some(line);
            } else if line.contains("\"id\":4") {
                recall_line = Some(line);
            }
            if ingest_line.is_some() && recall_line.is_some() {
                break;
            }
        }
    })
    .await
    .expect(
        "timed out — server likely refused to boot without embedder weights \
         (eager bootstrap probe still in place?)",
    );

    let tools_line = tools_line.expect(
        "server closed stdout before tools/list — bootstrap must succeed without \
         embedder weights (lazy embedder contract)",
    );
    assert!(
        tools_line.contains("memory.recall"),
        "tools/list must include memory.recall; line: {tools_line}"
    );

    // Behavior 2: ingest MUST succeed without any embedder — the KV + BM25
    // write needs no dense vector, so a missing embedder degrades vector
    // recall but must NOT break ingest (the lazy embedder tolerates the
    // NoopEmbedder fallback; the 2026-07-20 CI regression was ingest erroring
    // here). No `"error"` object in the JSON-RPC reply.
    let ingest_line = ingest_line.expect("server closed stdout before the ingest response");
    assert!(
        !ingest_line.contains(r#""error""#),
        "ingest without an embedder must succeed (KV + BM25 need no vectors); line: {ingest_line}"
    );

    // Behavior 3: the Direct recall must be an HONEST error naming the
    // embedder — not a silent `{{\"hits\":[]}}` success from zero vectors.
    let recall_line = recall_line.expect("server closed stdout before the recall response");
    let lower = recall_line.to_lowercase();
    assert!(
        lower.contains("embedder") || lower.contains("embedding"),
        "recall without any embedding backend must return an actionable error \
         naming the embedder; line: {recall_line}"
    );
    assert!(
        !recall_line.contains(r#""hits""#),
        "recall without any embedding backend must NOT silently succeed with a \
         hit list (mcp-recall-empty-hits bug class); line: {recall_line}"
    );

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server did not exit within 15s of stdin EOF")
        .expect("await child exit");
    assert!(status.success(), "server exited non-zero: {status:?}");
}
