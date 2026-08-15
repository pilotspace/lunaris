//! Emergency-drop CI gate (HOOK-06).
//!
//! Verifies that when the ingest call stalls beyond the configured timeout,
//! `lunaris-hook`:
//! 1. Exits with code 0 (does NOT block the Claude Code tool invocation).
//! 2. Emits a single-line JSON to stderr containing `"event":"emergency_drop"`.
//! 3. Completes before the stall's full duration (proving the timeout fired).
//!
//! # Methodology
//!
//! Spawns the `lunaris-hook` binary as a subprocess (required to observe exit code
//! and captured stderr — in-process calls bypass `main.rs` logic entirely).
//!
//! Two env vars control the test:
//! - `LUNARIS_HOOK_DROP_AFTER_MS=50` — sets the ingest timeout to 50ms.
//! - `LUNARIS_TEST_STALL_MS=200` — injects a 200ms `tokio::time::sleep` INSIDE
//!   the timeout block in `main.rs`. This ensures the timeout fires after 50ms
//!   while the stall is still sleeping at 200ms.
//!
//! The stall uses `tokio::time::sleep` (not `std::thread::sleep`) so that the
//! `tokio::time::timeout` can cancel it at the `.await` point.
//!
//! # Warmup spawn
//!
//! On macOS the first subprocess invocation incurs a Gatekeeper/dylib signing
//! delay (400–900ms). The warmup spawn populates the OS page cache so that the
//! timed run reflects the warm-cache steady-state cost, mirroring the pattern
//! in lunaris-mcp/tests/cold_start.rs.
//!
//! # Wall-clock budget
//!
//! After warmup: the timed run must complete in < STALL_MS (200ms).
//! This proves the timeout fired (otherwise the process would sleep the full
//! 200ms stall before exiting, pushing elapsed past STALL_MS).
//!
//! # Safety invariants
//!
//! - The test uses `LUNARIS_HOOK_DROP_AFTER_MS=50` (well above the 10ms clamp floor).
//! - `LUNARIS_HOOK_DROP_AFTER_MS=1` MUST NOT appear in any test code (plan constraint).
//! - `kill_on_drop(true)` on the timing run ensures no zombie processes on panic.
//! - 5s safety timeout on child.wait() prevents hung CI.

use std::io::Write as _;
use std::time::{Duration, Instant};

use lunaris_test_harness::open_test_store;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::time::timeout;

/// A valid PreToolUse envelope with a unique event_id for this test.
const ENVELOPE: &str = r#"{"hook_event_name":"PreToolUse","session_id":"emergency-drop-test-session","cwd":"/tmp/emergency-drop-test","tool_name":"Edit","tool_input":{"path":"src/main.rs","content":"fn main() {}"},"event_id":"emergency-drop-fixed-event-id","timestamp":"2026-05-25T00:00:00Z"}"#;

/// Timeout used in this test — 50ms is above the 10ms clamp floor and below
/// the 200ms stall, so the timeout will reliably fire.
const DROP_AFTER_MS: u64 = 50;

/// Artificial stall injected inside the timeout block (in main.rs).
/// Must be > DROP_AFTER_MS so the timeout fires before the stall completes.
const STALL_MS: u64 = 200;

/// Wall-clock budget (post-warmup): process must complete before the full stall.
/// < STALL_MS proves the timeout fired; >= STALL_MS means the timeout did not fire.
const WALL_CLOCK_BUDGET_MS: u64 = STALL_MS - 1;

/// Build a tokio::process::Command with all required env vars for the timed run.
/// Returns a command ready for `.spawn()`.
///
/// 0.7.0 port off `memory://`: `store_url` comes from a harness fixture the
/// caller holds open for the children's lifetime.
fn make_timed_cmd(store_url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"));
    cmd.env("LUNARIS_HOOK_DROP_AFTER_MS", DROP_AFTER_MS.to_string())
        .env("LUNARIS_TEST_STALL_MS", STALL_MS.to_string())
        .env("LUNARIS_STORE_URL", store_url)
        .env("LUNARIS_HOOK_SCOPE", "emergency-drop-test")
        .env("LUNARIS_HOOK_LOG", "warn")
        // Non-existent path → fast NoopEmbedder/NoopReranker fallback (no mmap).
        .env("LUNARIS_EMBEDDER_DIR", "/dev/null/weights")
        .env("LUNARIS_RERANKER_DIR", "/dev/null/weights")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    cmd
}

/// Build a std::process::Command with all required env vars for stderr capture.
fn make_capture_cmd(store_url: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_lunaris-hook"));
    cmd.env("LUNARIS_HOOK_DROP_AFTER_MS", DROP_AFTER_MS.to_string())
        .env("LUNARIS_TEST_STALL_MS", STALL_MS.to_string())
        .env("LUNARIS_STORE_URL", store_url)
        .env("LUNARIS_HOOK_SCOPE", "emergency-drop-test")
        .env("LUNARIS_HOOK_LOG", "warn")
        .env("LUNARIS_EMBEDDER_DIR", "/dev/null/weights")
        .env("LUNARIS_RERANKER_DIR", "/dev/null/weights")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    cmd
}

#[tokio::test(flavor = "current_thread")]
async fn emergency_drop_exits_zero_with_warning() {
    // Resolved once, before the warmup spawn, so every child in this test —
    // warmup, timed run, capture run — talks to the SAME store, exactly as
    // the single `memory://` literal used to imply.
    let store = open_test_store().await;
    // ── Warmup spawn ──────────────────────────────────────────────────────────
    //
    // On macOS the first subprocess invocation incurs a Gatekeeper/dylib signing
    // delay (400–900ms). The warmup populates the OS page cache so the timed run
    // reflects steady-state cost. Mirrored from lunaris-mcp/tests/cold_start.rs.
    {
        let mut warmup = make_timed_cmd(store.url())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn warmup lunaris-hook");
        let _ = timeout(Duration::from_secs(5), warmup.wait()).await;
    }

    // ── Run 1: timing gate (warm cache) ───────────────────────────────────────
    let start = Instant::now();

    let mut child = make_timed_cmd(store.url())
        .spawn()
        .expect("failed to spawn lunaris-hook binary for timing run");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(ENVELOPE.as_bytes()).await.expect("write envelope to stdin");
        // Drop stdin → send EOF to the child.
    }

    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child must exit within 5s safety window (possible hung binary)")
        .expect("child.wait() must succeed");

    let elapsed = start.elapsed();

    // ── Assertion 1: wall-clock budget ────────────────────────────────────────
    //
    // elapsed < STALL_MS proves the timeout fired. If it didn't fire, the process
    // would sleep the full 200ms stall before proceeding, pushing elapsed >= STALL_MS.
    assert!(
        elapsed < Duration::from_millis(WALL_CLOCK_BUDGET_MS),
        "emergency-drop wall-clock budget exceeded: {}ms (budget: {}ms). \
         The timeout mechanism in main.rs did not fire — process ran the full stall duration.",
        elapsed.as_millis(),
        WALL_CLOCK_BUDGET_MS,
    );

    // ── Assertion 2: exit code 0 ───────────────────────────────────────────────
    assert_eq!(
        status.code(),
        Some(0),
        "emergency-drop must exit 0 (not block Claude Code). Got: {:?}",
        status.code(),
    );

    // ── Run 2: stderr capture ─────────────────────────────────────────────────
    //
    // std::process::Command::wait_with_output() collects all piped stderr bytes
    // after the process exits. A second run is needed because tokio::process::Child
    // does not expose piped stderr bytes after wait().
    let mut cmd2 = make_capture_cmd(store.url());
    let mut child2 = cmd2.spawn().expect("spawn stderr-capture run");
    if let Some(mut stdin2) = child2.stdin.take() {
        stdin2.write_all(ENVELOPE.as_bytes()).expect("write stdin for run 2");
    }
    let output = child2.wait_with_output().expect("wait_with_output for run 2");

    // ── Assertion 3: exit code 0 on stderr-capture run ───────────────────────
    assert_eq!(
        output.status.code(),
        Some(0),
        "run 2: emergency-drop must exit 0. Got: {:?}",
        output.status.code(),
    );

    // ── Assertion 4: stderr contains emergency_drop JSON ─────────────────────
    let stderr_text = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr_text.contains("emergency_drop"),
        "stderr must contain 'emergency_drop' JSON warning. Got:\n{stderr_text}",
    );

    // Find and validate the JSON line.
    let drop_line = stderr_text
        .lines()
        .find(|l| l.contains("emergency_drop"))
        .expect("must find the emergency_drop line in stderr");

    let drop_json: serde_json::Value =
        serde_json::from_str(drop_line).expect("emergency_drop line must be valid JSON");

    assert_eq!(
        drop_json["level"].as_str(),
        Some("warn"),
        "emergency_drop JSON must have level=warn"
    );
    assert_eq!(
        drop_json["event"].as_str(),
        Some("emergency_drop"),
        "emergency_drop JSON must have event=emergency_drop"
    );
    assert!(
        drop_json["reason"].as_str().unwrap_or("").contains("ingest_timeout"),
        "emergency_drop JSON reason must contain 'ingest_timeout'. Got: {}",
        drop_json["reason"],
    );
    assert!(
        drop_json["kind"].as_str().is_some(),
        "emergency_drop JSON must have a 'kind' field. Got: {drop_json}"
    );

    println!(
        "HOOK-06 emergency-drop PASS: exit=0, elapsed={}ms (< {}ms budget), \
         stderr JSON: {drop_line}",
        elapsed.as_millis(),
        WALL_CLOCK_BUDGET_MS,
    );
}
