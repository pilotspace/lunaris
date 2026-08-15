//! SessionStart additionalContext injection (session-context-inject task).
//!
//! E2E tier: spawns the real binary with stdout PIPED (unlike
//! session_switch.rs, stdout is the contract surface here).
//!
//! ## Backend
//!
//! Every path here takes a disposable child-process Moon from
//! `lunaris-test-harness`. There are no arms and no skips: the positive path
//! was gated behind `LUNARIS_HOOK_TEST_MOON_URL` (which nothing in CI ever set,
//! so it ran zero times) and the silent-skip path was pinned to the embedded
//! backend on a mistaken premise — see each test's doc comment. The env hatch
//! is deliberately not carried over: it pointed a WRITING test at an
//! operator's live store.
//!
//! Red evidence: handover.rs ships as stubs until the build phase — the
//! NotSupported warn and the stdout JSON do not exist yet.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use lunaris_test_harness::open_test_store;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

const SCOPE: &str = "ctx-inject-e2e";

const START_A: &str = r#"{
  "hook_event_name": "SessionStart",
  "session_id": "sess-a",
  "cwd": "/tmp/test-hook-repo"
}"#;

const START_B: &str = r#"{
  "hook_event_name": "SessionStart",
  "session_id": "sess-b",
  "cwd": "/tmp/test-hook-repo"
}"#;

const PRE_TOOL_USE: &str = r#"{
  "hook_event_name": "PreToolUse",
  "session_id": "sess-b",
  "cwd": "/tmp/test-hook-repo",
  "tool_name": "Bash",
  "tool_input": {"command": "ls"}
}"#;

async fn run_hook(
    envelope: &str,
    store_url: &str,
    scopes: &Path,
    sessions: &Path,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_STORE_URL", store_url)
        .env("LUNARIS_HOOK_SCOPE", SCOPE)
        .env("LUNARIS_SCOPES_FILE", scopes.to_str().unwrap())
        .env("LUNARIS_SESSIONS_FILE", sessions.to_str().unwrap())
        .env("LUNARIS_HOOK_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn lunaris-hook");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(envelope.as_bytes()).await.unwrap();
    drop(stdin);
    timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("hook must not hang")
        .expect("wait")
}

/// Same-session restart: no switch -> stdout EMPTY both times (regression pin
/// for the stdout-discipline Must).
#[tokio::test(flavor = "multi_thread")]
async fn e2e_same_session_restart_stdout_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes = tmp.path().join("scopes.json");
    let sessions = tmp.path().join("sessions.json");

    let store = open_test_store().await;

    for _ in 0..2 {
        let out = run_hook(START_A, store.url(), &scopes, &sessions).await;
        assert_eq!(out.status.code(), Some(0));
        assert!(
            out.stdout.is_empty(),
            "same-session restart must emit NOTHING on stdout, got: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// Non-SessionStart events never touch stdout.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_non_session_start_stdout_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes = tmp.path().join("scopes.json");
    let sessions = tmp.path().join("sessions.json");

    let store = open_test_store().await;
    let out = run_hook(PRE_TOOL_USE, store.url(), &scopes, &sessions).await;
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "non-SessionStart events must emit NOTHING on stdout, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A switch whose previous pad yields nothing must FAIL SILENT — stdout empty,
/// ONE stderr warn naming the skip, exit 0.
///
/// **Re-expressed in 0.7.0.** This was pinned to the embedded backend on the
/// theory that its `NotSupported` `keyword_search` was what forced the skip.
/// That was never the mechanism: `handover::build_handover_context` enumerates
/// the pad with a KV `scan_range`, not a keyword search, and this test never
/// seeded a pad — so the branch it actually exercised was "previous pad is
/// empty", which every backend reaches. It now says so, runs on the real
/// substrate, and asserts WHICH branch fired so it cannot start passing for an
/// unrelated reason.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_switch_with_empty_pad_warns_and_stays_silent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes = tmp.path().join("scopes.json");
    let sessions = tmp.path().join("sessions.json");

    // Deliberately NOT seeded: sess-a's pad is empty at switch time.
    let store = open_test_store().await;

    let out_a = run_hook(START_A, store.url(), &scopes, &sessions).await;
    assert_eq!(out_a.status.code(), Some(0));

    let out_b = run_hook(START_B, store.url(), &scopes, &sessions).await;
    assert_eq!(out_b.status.code(), Some(0), "context failure must not change the exit code");
    assert!(
        out_b.stdout.is_empty(),
        "failed enumeration must emit NOTHING on stdout, got: {}",
        String::from_utf8_lossy(&out_b.stdout)
    );
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("context_inject skipped"),
        "the silent skip must leave one stderr warn naming context_inject, got: {stderr_b}"
    );
    assert!(
        stderr_b.contains("previous pad is empty"),
        "the skip must be the EMPTY-PAD branch — if this starts reading \"enumeration \
         failed\" the fixture is broken, not the contract: {stderr_b}"
    );
}

/// Positive path on a real Moon: seed pad entries for sess-a, flip the session
/// to sess-b, and assert the stdout hook-JSON carries the distilled summary.
///
/// 0.7.0: previously gated on `LUNARIS_HOOK_TEST_MOON_URL`, which nothing in
/// CI ever set — so this ran zero times. Slice 2 gave it a harness-issued
/// ephemeral Moon behind a "did we get Moon?" guard; with the embedded backend
/// deleted that guard can no longer be false, so it is gone too. This test now
/// runs, unconditionally, on every invocation.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_switch_on_moon_injects_additional_context() {
    let store = open_test_store().await;
    let moon_url = store.url().to_owned();

    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes = tmp.path().join("scopes.json");
    let sessions = tmp.path().join("sessions.json");

    // Session A becomes active.
    let out_a = run_hook(START_A, &moon_url, &scopes, &sessions).await;
    assert_eq!(out_a.status.code(), Some(0));

    // Seed sess-a's pad the way the MCP server writes it: Episodes whose
    // source is "scratchpad/sess-a/{key}" with the JSON value as content.
    {
        use lunaris_core::{Episode, HlcClock, NoopEmbedder, Scope};
        let storage = lunaris::open(&moon_url).await.expect("open live Moon");
        let scope = Scope::new(SCOPE).unwrap();
        let clock = HlcClock::new(0);
        let embedder = NoopEmbedder::default();
        for (k, v) in [("plan", "\"ship task 3\""), ("blocker", "\"none\"")] {
            let ep =
                Episode::new(scope.clone(), format!("scratchpad/sess-a/{k}"), v.to_owned(), &clock);
            lunaris_ingest::ingest_episode(storage.as_ref(), &embedder, &clock, ep)
                .await
                .expect("seed pad episode");
        }
    }

    // Switch to session B: the hook must inject the summary on stdout.
    let out_b = run_hook(START_B, &moon_url, &scopes, &sessions).await;
    assert_eq!(out_b.status.code(), Some(0));
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout_b.trim()).unwrap_or_else(|e| {
        panic!("switch on Moon must emit one stdout JSON object, got ({e}): {stdout_b}")
    });
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart", "hook JSON shape: {v}");
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext must be a string");
    assert!(ctx.contains("sess-a"), "context must name the previous session: {ctx}");
    assert!(ctx.contains("plan"), "context must list the pad keys: {ctx}");
}
