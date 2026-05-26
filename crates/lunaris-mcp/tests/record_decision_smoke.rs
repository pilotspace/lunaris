//! Smoke test for `memory.record_decision`.
//!
//! Spawns the `lunaris-mcp` binary in a subprocess, drives it through the MCP
//! stdio transport (newline-delimited JSON-RPC), and asserts:
//!   1. `initialize` handshake succeeds.
//!   2. `tools/list` returns a tools array that includes `memory.record_decision`.
//!   3. `tools/call` for `memory.record_decision` returns a non-empty `lsn`.
//!   4. The response does NOT contain a JSON-RPC `error` field.
//!
//! Uses a temp-dir SQLite DB so the test is hermetic and leaves no state behind.
//!
//! # Transport note
//! rmcp's stdio transport uses NDJSON — one JSON object per line —
//! NOT Content-Length framing.
//!
//! # RED phase
//! Before Task 2 registers `memory.record_decision`, the `tools/list` assertion
//! at step 2 will fail because the tool is not yet in the list.

use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Send one JSON-RPC message as a newline-terminated line (rmcp NDJSON format).
async fn send_msg(stdin: &mut tokio::process::ChildStdin, msg: &serde_json::Value) {
    let mut line = serde_json::to_string(msg).expect("serialize msg");
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.expect("write msg");
    stdin.flush().await.expect("flush");
}

/// Read one NDJSON line from the server stdout, skipping non-JSON lines and
/// rmcp Parse Error responses (code -32700, no `id`).
async fn read_msg<R>(reader: &mut BufReader<R>) -> serde_json::Value
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.expect("read line");
        if n == 0 {
            panic!("EOF on server stdout — server exited unexpectedly");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => {
                // Skip pure Parse Error responses from rmcp (no `id` field,
                // error code -32700) — these are informational and not our response.
                let is_parse_err = v.get("id").is_none()
                    && v.get("error").and_then(|e| e.get("code")).and_then(|c| c.as_i64())
                        == Some(-32700);
                if is_parse_err {
                    continue;
                }
                return v;
            }
            Err(_) => {
                // Non-JSON line — ignore (tracing output, etc.)
                continue;
            }
        }
    }
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn record_decision_smoke() {
    // Locate binary — cargo sets CARGO_BIN_EXE_<name> for integration tests.
    // When the env var is absent fall back to the standard debug build path.
    let bin = std::env::var("CARGO_BIN_EXE_lunaris-mcp").unwrap_or_else(|_| {
        let manifest = env!("CARGO_MANIFEST_DIR");
        format!("{manifest}/../../target/debug/lunaris-mcp")
    });

    let tmp = tempfile::tempdir().expect("temp dir");
    let db_path = tmp.path().join("record_decision_smoke.db");
    let storage = format!("sqlite://{}", db_path.display());

    let mut child = Command::new(&bin)
        .env("LUNARIS_MCP_SCOPE", "test-record-decision")
        .env("LUNARIS_MCP_STORAGE", &storage)
        // Skip embedder health probe — ingest-only test, no recall exercised.
        // Without this, bootstrap rejects NoopEmbedder and the server exits
        // before responding (mcp-recall-empty-hits fix; see state.rs).
        .env("LUNARIS_MCP_SKIP_EMBEDDER_PROBE", "1")
        .env("LUNARIS_MCP_LOG", "error")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"));

    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);

    // ── 1. Send initialize ────────────────────────────────────────────────────
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.0.0" }
        }
    });
    send_msg(&mut stdin, &init_req).await;

    // First-run SQLite migrations can take time on a cold DB.
    let init_val = timeout(Duration::from_secs(60), read_msg(&mut reader))
        .await
        .expect("initialize timed out (first-run DB migration can take up to 60 s)");

    assert!(init_val["error"].is_null(), "initialize returned error: {init_val}");
    let proto = &init_val["result"]["protocolVersion"];
    assert!(!proto.is_null(), "missing protocolVersion in initialize result");

    // Send initialized notification (required by MCP protocol).
    let initialized_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    send_msg(&mut stdin, &initialized_notif).await;

    // ── 2. tools/list — assert memory.record_decision is registered ───────────
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    send_msg(&mut stdin, &list_req).await;

    let list_val = timeout(Duration::from_secs(10), read_msg(&mut reader))
        .await
        .expect("tools/list timed out");

    assert!(list_val["error"].is_null(), "tools/list returned error: {list_val}");

    let tools = list_val["result"]["tools"].as_array().expect("tools array");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // RED: this assertion FAILS before Task 2 registers the tool.
    assert!(
        tool_names.contains(&"memory.record_decision"),
        "memory.record_decision not in tools/list: {tool_names:?}"
    );

    // ── 3. tools/call memory.record_decision ─────────────────────────────────
    let call_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "memory.record_decision",
            "arguments": {
                "decision": "Use SQLite for local dev",
                "rationale": "Zero dependencies.",
                "tags": ["arch", "storage"]
            }
        }
    });
    send_msg(&mut stdin, &call_req).await;

    let call_val = timeout(Duration::from_secs(30), read_msg(&mut reader))
        .await
        .expect("tools/call timed out");

    assert!(call_val["error"].is_null(), "tools/call returned JSON-RPC error: {call_val}");

    // The MCP result content array; the first text item carries the JSON output.
    let content = &call_val["result"]["content"];
    assert!(!content.is_null(), "missing content in tools/call result: {call_val}");

    let text = content
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|item| item["text"].as_str())
        .unwrap_or_else(|| panic!("expected text content item: {call_val}"));

    let output: serde_json::Value =
        serde_json::from_str(text).expect("parse record_decision response JSON");

    // Assert lsn is non-empty with wall_ms:counter format.
    let lsn = output["lsn"].as_str().expect("lsn field in record_decision response");
    assert!(!lsn.is_empty(), "lsn must be non-empty, got: {output}");
    let parts: Vec<&str> = lsn.split(':').collect();
    assert_eq!(parts.len(), 2, "lsn must be 'wall_ms:counter', got: {lsn}");
    parts[0].parse::<u64>().unwrap_or_else(|_| panic!("lsn wall_ms not u64: {lsn}"));
    parts[1].parse::<u32>().unwrap_or_else(|_| panic!("lsn counter not u32: {lsn}"));

    // Graceful shutdown.
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}
