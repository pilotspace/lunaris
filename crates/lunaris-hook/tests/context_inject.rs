//! SessionStart additionalContext injection (session-context-inject task).
//!
//! E2E tier: spawns the real binary with stdout PIPED (unlike
//! session_switch.rs, stdout is the contract surface here).
//! ## Backend (0.7.0 port off `memory://`)
//!
//! - the two stdout-discipline paths take whatever store `lunaris-test-harness`
//!   resolves (ephemeral child-process Moon, else `memory://`) — they assert
//!   only that stdout stays empty, which is backend-independent;
//! - `e2e_switch_on_memory_warns_and_stays_silent` is a DEGRADE PIN and stays
//!   on the embedded backend by explicit `Policy::ForceMemory`: its whole
//!   subject is the `keyword_search` `NotSupported` skip, which Moon does not
//!   produce. 0.7.0 must re-express it against a stubbed `KeywordPort`;
//! - the positive path was gated behind `LUNARIS_HOOK_TEST_MOON_URL`, which
//!   nothing in CI ever set, so it was skipped on every run since it was
//!   written. It now takes a harness-issued ephemeral Moon and actually
//!   executes whenever one resolves. The env hatch is deliberately not carried
//!   over: it pointed a writing test at an operator's live store.
//!
//! Red evidence: handover.rs ships as stubs until the build phase — the
//! NotSupported warn and the stdout JSON do not exist yet.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use lunaris_test_harness::{Backend, Policy, open_test_store, open_test_store_with};
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

/// Switch on the embedded backend: keyword_search is NotSupported there, so
/// the injection must FAIL SILENT — stdout empty, ONE stderr warn naming the
/// skip, exit 0 (decided by the ingest alone).
///
/// **Degrade pin — deliberately NOT ported to Moon.** The subject of the test
/// IS the `NotSupported` skip; Moon implements `keyword_search` and takes the
/// injecting path instead. Routed through the harness with an explicit
/// `Policy::ForceMemory` so 0.7.0's deletion of the embedded backend surfaces
/// here rather than silently inverting the assertion.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_switch_on_memory_warns_and_stays_silent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes = tmp.path().join("scopes.json");
    let sessions = tmp.path().join("sessions.json");

    let store = open_test_store_with(Policy::ForceMemory).await;

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
        stderr_b.contains("context_inject"),
        "the silent skip must leave one stderr warn naming context_inject, got: {stderr_b}"
    );
}

/// Positive path on a real Moon: seed pad entries for sess-a, flip the session
/// to sess-b, and assert the stdout hook-JSON carries the distilled summary.
///
/// 0.7.0 port: previously gated on `LUNARIS_HOOK_TEST_MOON_URL`, which nothing
/// in CI ever set — so this ran zero times. It now takes a harness-issued
/// ephemeral Moon and skips only when the harness resolved the embedded
/// backend (no Moon binary, or `LUNARIS_TEST_BACKEND=memory`), which is the
/// one case where the assertion below is genuinely unreachable.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_switch_on_moon_injects_additional_context() {
    let store = open_test_store().await;
    if store.backend() != Backend::Moon {
        eprintln!("skipping: harness resolved the embedded backend, not Moon");
        return;
    }
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
