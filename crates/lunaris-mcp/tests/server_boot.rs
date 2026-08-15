//! Integration guard: the MCP server actually BOOTS and registers every tool.
//!
//! rmcp 1.7 validates each `#[tool]`'s generated `outputSchema` when it builds
//! the tool router, and ABORTS the process (panic, exit 101) if any tool whose
//! handler returns `Json<T>` produces a schema whose root is not `type:"object"`
//! (e.g. a `#[serde(tag = ...)]` enum, whose root is `oneOf`). The unit tests
//! call each tool's `handle()`/`handle_inner()` directly and NEVER construct the
//! router, so they cannot catch this class of bug — a green unit suite does not
//! prove the server can start (see fix `89b9181`, found by dogfooding into Codex).
//!
//! This test spawns the real binary, drives the MCP `initialize` + `tools/list`
//! handshake over stdio, and asserts the server stays up and enumerates all 16
//! tools. It exercises router validation for EVERY tool, not one response type,
//! so reintroducing a non-object response schema on ANY future tool fails here.
//!
//! Backend is whatever `lunaris-test-harness` resolves — an ephemeral
//! child-process Moon, degrading to `memory://` where no Moon binary exists
//! (0.7.0 port off the hard-coded scheme). No GGUF is required either way; the
//! embedder is lazy and never loads here, so the test stays light and runs
//! under the default feature set in CI. Note the harness SPAWNS a prebuilt
//! `moon` binary — it does not enable `embedded-moon`, so this test binary
//! still never links the Moon server (CLAUDE.md invariant).

use std::process::Stdio;
use std::time::Duration;

use lunaris_test_harness::open_test_store;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Every tool the server must register. If the router build aborts on a bad
/// schema, NONE of these appear (the process dies before answering tools/list).
const EXPECTED_TOOLS: &[&str] = &[
    "memory.ingest",
    "memory.recall",
    "memory.forget",
    "memory.list_scopes",
    "memory.record_decision",
    "memory.record_edit",
    "memory.feedback",
    "memory.status",
    "memory.scratchpad_write",
    "memory.scratchpad_read",
    "memory.scratchpad_grep",
    "memory.scratchpad_consolidate",
    "memory.verify_agenda",
    "memory.resolve",
    "memory.dream_agenda",
    "memory.distill",
];

#[tokio::test]
async fn server_boots_and_lists_all_tools() {
    let bin = env!("CARGO_BIN_EXE_lunaris-mcp");
    // Bound for the child's whole lifetime — it owns the Moon.
    let store = open_test_store().await;

    let mut child = Command::new(bin)
        .arg("--storage")
        .arg(store.url())
        .env("LUNARIS_MCP_SKIP_STAGE", "1")
        .env("LUNARIS_MCP_SCOPE", "ci-boot-test")
        .env("LUNARIS_MCP_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lunaris-mcp binary");

    let mut stdin = child.stdin.take().expect("child stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("child stdout")).lines();

    // Newline-delimited JSON-RPC: initialize -> initialized -> tools/list.
    let handshake = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"ci","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        "\n",
    );
    stdin.write_all(handshake.as_bytes()).await.expect("write handshake");
    stdin.flush().await.expect("flush handshake");

    // Scan stdout for the tools/list response (id:2). If the router build
    // aborted at startup, stdout closes (next_line -> None) and we fail loudly.
    let tools_line = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await.expect("read stdout line") {
            if line.contains("\"id\":2") {
                return Some(line);
            }
        }
        None
    })
    .await
    .expect("timed out waiting for tools/list — server likely aborted at router build (outputSchema panic)")
    .expect("server closed stdout before answering tools/list — startup panic (invalid tool outputSchema?)");

    for tool in EXPECTED_TOOLS {
        assert!(
            tools_line.contains(tool),
            "tools/list is missing `{tool}` — a tool was dropped or failed schema validation.\nline: {tools_line}"
        );
    }

    // Clean shutdown: closing stdin drives the rmcp loop to EOF; the process
    // must exit 0 (not 101). A non-zero exit signals a startup/shutdown defect.
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server did not exit within 15s of stdin EOF")
        .expect("await child exit");
    assert!(status.success(), "server exited non-zero: {status:?}");
}
