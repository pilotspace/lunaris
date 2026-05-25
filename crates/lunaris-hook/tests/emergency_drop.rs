//! Emergency-drop CI gate (HOOK-06).
//!
//! Verifies that when the ingest call stalls beyond the configured timeout,
//! `lunaris-hook`:
//! 1. Exits with code 0 (does NOT block the Claude Code tool invocation).
//! 2. Emits a single-line JSON to stderr containing `"event":"emergency_drop"`.
//! 3. The process exits well within the wall-clock budget.
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
//! # Safety invariants
//!
//! - The test uses `LUNARIS_HOOK_DROP_AFTER_MS=50` (well above the 10ms clamp floor).
//! - `LUNARIS_HOOK_DROP_AFTER_MS=1` MUST NOT appear in any test code (plan constraint).
//! - `kill_on_drop(true)` on the timing run ensures no zombie processes on panic.
//! - 5s safety timeout on child.wait() prevents hung CI.

use std::io::Write as _;
use std::time::{Duration, Instant};

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

/// Wall-clock budget for the timed run: DROP_AFTER_MS + 100ms for OS scheduling.
const WALL_CLOCK_BUDGET_MS: u64 = DROP_AFTER_MS + 100;

/// Shared env builder for both the timed and stderr-capture runs.
fn make_cmd() -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_lunaris-hook"));
    cmd.env("LUNARIS_HOOK_DROP_AFTER_MS", DROP_AFTER_MS.to_string())
        .env("LUNARIS_TEST_STALL_MS", STALL_MS.to_string())
        .env("LUNARIS_STORE_URL", "memory://")
        .env("LUNARIS_HOOK_SCOPE", "emergency-drop-test")
        .env("LUNARIS_HOOK_LOG", "warn")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    cmd
}

#[tokio::test(flavor = "current_thread")]
async fn emergency_drop_exits_zero_with_warning() {
    // ── Run 1: timing gate (async spawn + kill_on_drop safety) ───────────────
    //
    // Use tokio::process::Command for kill_on_drop support and async wait.
    let start = Instant::now();

    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_HOOK_DROP_AFTER_MS", DROP_AFTER_MS.to_string())
        .env("LUNARIS_TEST_STALL_MS", STALL_MS.to_string())
        .env("LUNARIS_STORE_URL", "memory://")
        .env("LUNARIS_HOOK_SCOPE", "emergency-drop-test")
        .env("LUNARIS_HOOK_LOG", "warn")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn lunaris-hook binary for timing run");

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(ENVELOPE.as_bytes())
            .await
            .expect("write envelope to stdin");
        // Drop stdin → send EOF.
    }

    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child must exit within 5s safety window (possible hung binary)")
        .expect("child.wait() must succeed");

    let elapsed = start.elapsed();

    // ── Assertion 1: wall-clock budget ────────────────────────────────────────
    assert!(
        elapsed < Duration::from_millis(WALL_CLOCK_BUDGET_MS),
        "emergency-drop wall-clock budget exceeded: {}ms (budget: {}ms). \
         The timeout mechanism in main.rs did not fire within the expected window.",
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

    // ── Run 2: stderr capture (std::process for wait_with_output) ────────────
    //
    // tokio::process::Child does not expose wait_with_output that gives captured
    // stderr after piping. Use std::process for the stderr-content assertions.
    let mut child2 = make_cmd().spawn().expect("spawn stderr-capture run");
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

    // Validate the JSON is parseable and has the required fields.
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
        "HOOK-06 emergency-drop PASS: exit=0, timing_run={}ms, stderr JSON: {drop_line}",
        elapsed.as_millis()
    );
}
