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
//! handshake over stdio, and asserts the server stays up and enumerates all 17
//! tools. It exercises router validation for EVERY tool, not one response type,
//! so reintroducing a non-object response schema on ANY future tool fails here.
//!
//! Backend is a `lunaris-test-harness` ephemeral child-process Moon. There is
//! still no shipped storage default; storage comes either from `--storage` or
//! (task #28) from a live store `lunaris-contextd` advertises in
//! `~/.lunaris/contextd-moon.url`. Both arms are pinned below —
//! `no_storage_refuses_to_boot_with_the_quickstart` for the refusal,
//! `advertised_contextd_store_boots_the_stock_server` for the discovery path.
//! No GGUF is required; the embedder is
//! lazy and never loads here, so the test stays light and runs under the
//! default feature set in CI. Note the harness SPAWNS a prebuilt `moon`
//! binary — it does not enable `embedded-moon`, so this test binary still
//! never links the Moon server (CLAUDE.md invariant).

use std::process::Stdio;
use std::time::Duration;

use futures::StreamExt;
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
    "memory.profile",
    "memory.remember",
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
    "memory.retention",
    "memory.retention_enforce",
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

    // Wave 6: the loop above is a PRESENCE check, and a presence check passes a
    // superset — a tool registered in `main.rs` and never added here was
    // invisible to it, and to every roster page that copies from here. Count
    // what the server actually answered with.
    let registered = tools_line.matches("\"memory.").count();
    assert_eq!(
        registered,
        EXPECTED_TOOLS.len(),
        "the server registers {registered} `memory.*` tools but this guard enumerates {}. \
         A tool added to main.rs without a line here is unguarded, and the docs guard \
         (scripts/tests/test_mcp_tool_count_matches_the_server.py) reads main.rs, not this list, \
         so the two can disagree silently.\nline: {tools_line}",
        EXPECTED_TOOLS.len()
    );

    // Clean shutdown: closing stdin drives the rmcp loop to EOF; the process
    // must exit 0 (not 101). A non-zero exit signals a startup/shutdown defect.
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server did not exit within 15s of stdin EOF")
        .expect("await child exit");
    assert!(status.success(), "server exited non-zero: {status:?}");
}

/// 0.7.0 + task #28: with no `--storage`, no `LUNARIS_MCP_STORAGE`, **and no
/// live contextd store advertised**, the server must REFUSE to boot and say
/// what to do about it.
///
/// The shipped default was a per-scope SQLite file. Deleting the backend
/// without deleting the default would have left the binary "starting" against
/// a URL every tool call then fails on — the worst outcome for a stdio server
/// whose client shows tool errors, not startup logs.
///
/// Driven against the real binary with an EMPTY env for both spellings, so it
/// also proves the clap `env =` fallback does not resurrect a value from the
/// developer's shell.
///
/// `HOME` is an EMPTY tempdir. Since task #28 arm 2 reads
/// `~/.lunaris/contextd-moon.url`, the refusal is now conditional on there
/// being nothing advertised — without the tempdir this test would flake (pass
/// or fail) depending on whether the developer running it has `lunaris-contextd`
/// up, and would stop pinning anything.
///
/// Skipped under `--features embedded-moon`: that build DOES have a default
/// (the Moon it launches in-process), and it is a dev/test build that never
/// ships (CLAUDE.md invariant).
#[cfg(not(feature = "embedded-moon"))]
#[tokio::test]
async fn no_storage_refuses_to_boot_with_the_quickstart() {
    let bin = env!("CARGO_BIN_EXE_lunaris-mcp");
    let home = tempfile::tempdir().expect("tempdir for HOME");

    let out = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(bin)
            .env_remove("LUNARIS_MCP_STORAGE")
            // See the note in `advertised_contextd_store_boots_the_stock_server`:
            // LUNARIS_CONTEXTD_SOCKET outranks the tempdir HOME.
            .env_remove("LUNARIS_CONTEXTD_SOCKET")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("LUNARIS_MCP_SKIP_STAGE", "1")
            .env("LUNARIS_MCP_SCOPE", "ci-no-storage-test")
            .env("LUNARIS_MCP_LOG", "error")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("binary must fail fast, not hang waiting on stdio")
    .expect("run lunaris-mcp binary");

    assert!(!out.status.success(), "a storage-less start must not succeed: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    for needle in [
        "LUNARIS_MCP_STORAGE",
        "--storage",
        "moon://",
        "--shards 1",
        "docs/operations/external-moon.md",
        "lunaris-migrate",
        // Task #28: starting contextd is a supported alternative to --storage,
        // and the refusal is the only place an operator will learn that.
        "lunaris-contextd",
        "contextd-moon.url",
    ] {
        assert!(stderr.contains(needle), "startup refusal must mention {needle}:\n{stderr}");
    }
    // Nothing was advertised here, so the stale-file note would be a lie.
    assert!(
        !stderr.contains("did not answer a RESP PING"),
        "no discovery file existed — the stale-file note must not appear:\n{stderr}"
    );
    // The dead default must not be advertised anywhere in the refusal except
    // as history — never as something the operator could still reach for.
    assert!(
        !stderr.contains("sqlite:///<HOME>"),
        "the retired SQLite default must not be offered as a way out:\n{stderr}"
    );
}

/// Task #28: an ADVERTISED, PROBED store is not a guessed default.
///
/// A stock build with no `--storage` and no `LUNARIS_MCP_STORAGE` must adopt
/// the Moon `lunaris-contextd` advertises in `~/.lunaris/contextd-moon.url`,
/// exactly as `lunaris-hook` already does — and must SERVE against that store,
/// not merely boot.
///
/// The discriminating half is the readback: the test connects to the SAME Moon
/// out-of-band and scans the scope's episode keyspace for the value the server
/// wrote over MCP. A server that booted but opened some other store leaves that
/// scan empty, so "it started" cannot pass for "it is wired".
///
/// Hermetic: `HOME` points at a tempdir, so the discovery file under test is
/// the only one on the search path, and `LUNARIS_CONTEXTD_SOCKET` is cleared so
/// a developer machine running a real contextd cannot leak into (or out of)
/// this test through the proxy path either.
///
/// Skipped under `--features embedded-moon`: that build launches its own
/// in-process Moon before any discovery could apply (dev/test only — CLAUDE.md
/// invariant keeps it out of every shipped binary).
#[cfg(not(feature = "embedded-moon"))]
#[tokio::test]
async fn advertised_contextd_store_boots_the_stock_server() {
    use lunaris_core::{Scope, StoragePort, keyspace};

    const SCOPE: &str = "ci-discovery-boot-test";

    let bin = env!("CARGO_BIN_EXE_lunaris-mcp");
    // Bound for the child's whole lifetime — it owns the Moon contextd would
    // have owned in production.
    let store = open_test_store().await;

    // The discovery file contextd writes when its embedded Moon is ready.
    let home = tempfile::tempdir().expect("tempdir for HOME");
    let lunaris_dir = home.path().join(".lunaris");
    std::fs::create_dir_all(&lunaris_dir).expect("create ~/.lunaris");
    std::fs::write(lunaris_dir.join("contextd-moon.url"), format!("{}\n", store.url()))
        .expect("write discovery file");

    let mut child = Command::new(bin)
        // NO --storage, NO LUNARIS_MCP_STORAGE: the discovery file is the only
        // thing standing between this server and the boot refusal.
        .env_remove("LUNARIS_MCP_STORAGE")
        // `proxy.rs` reads LUNARIS_CONTEXTD_SOCKET *ahead* of `$HOME/.lunaris`,
        // so the tempdir HOME alone does not isolate us: a developer with that
        // var exported would route these tool calls into their own contextd's
        // store and the readback below would scan an empty keyspace.
        .env_remove("LUNARIS_CONTEXTD_SOCKET")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LUNARIS_MCP_SKIP_STAGE", "1")
        .env("LUNARIS_MCP_SCOPE", SCOPE)
        .env("LUNARIS_MCP_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lunaris-mcp binary");

    let mut stdin = child.stdin.take().expect("child stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("child stdout")).lines();

    // `scratchpad_write` is the cheapest tool that touches storage AND does not
    // need an embedder (WorkingMemory::write rides ingest, not recall), so the
    // child never loads a GGUF.
    let marker = "discovery-boot-marker";
    let handshake = format!(
        concat!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"ci","version":"0"}}}}}}"#,
            "\n",
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#,
            "\n",
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory.scratchpad_write","arguments":{{"key":"{marker}","value":"advertised-store"}}}}}}"#,
            "\n",
        ),
        marker = marker,
    );
    stdin.write_all(handshake.as_bytes()).await.expect("write handshake");
    stdin.flush().await.expect("flush handshake");

    let call_line = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.expect("read stdout line") {
            if line.contains("\"id\":2") {
                return Some(line);
            }
        }
        None
    })
    .await
    .expect("timed out waiting for the scratchpad_write response")
    .expect(
        "server closed stdout before answering — a stock build refused to boot despite a live \
         contextd store being advertised in ~/.lunaris/contextd-moon.url",
    );
    assert!(
        !call_line.contains("\"error\""),
        "memory.scratchpad_write failed on the discovery-resolved store: {call_line}"
    );

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server did not exit within 15s of stdin EOF")
        .expect("await child exit");
    assert!(status.success(), "server exited non-zero: {status:?}");

    // ── The discriminating assertion ────────────────────────────────────────
    // Read the ADVERTISED Moon out-of-band. If the server had resolved any
    // other store (or a default), this scan is empty.
    let scope = Scope::new(SCOPE).expect("valid scope");
    let storage = lunaris_storage_moon::MoonStorage::connect(store.url())
        .await
        .expect("connect to the advertised Moon");
    let prefix = keyspace::episode_prefix(&scope);
    let mut stream =
        storage.scan_range(&scope, &prefix, None).await.expect("scan the episode keyspace");
    let mut found = false;
    while let Some(item) = stream.next().await {
        let (_key, value) = item.expect("scan item");
        if String::from_utf8_lossy(&value).contains(marker) {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "the MCP server booted but wrote nowhere this Moon can see — a boot that ignores the \
         advertised contextd store is exactly the split-routing failure task #20 contained"
    );
}
