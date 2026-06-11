//! Session-switch detection via the durable marker (session-switch-detect task).
//!
//! Unit tier drives the `session_marker` API against a temp file; the e2e
//! tier spawns the real binary twice and asserts the stderr switch line.
//! Red evidence: the marker module ships as stubs until the build phase.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use lunaris_hook::session_marker::{
    SwitchObserved, observe_end_at, observe_start_at, read_active_at, sanitize_session_id,
    switch_meta,
};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

fn temp_sessions_file() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("sessions.json");
    (tmp, path)
}

// ── Unit tier: marker behavior ────────────────────────────────────────────────

#[test]
fn first_session_no_switch() {
    let (_tmp, path) = temp_sessions_file();
    let switch = observe_start_at(&path, "scope-a", "sess-1");
    assert_eq!(switch, None, "first session ever must not report a switch");
    assert_eq!(
        read_active_at(&path, "scope-a"),
        Some(("sess-1".to_owned(), false)),
        "the first session must become the active marker"
    );
}

#[test]
fn crash_switch_detected_without_session_end() {
    let (_tmp, path) = temp_sessions_file();
    assert_eq!(observe_start_at(&path, "scope-a", "sess-1"), None);
    // No SessionEnd (crash / kill) — the next start alone must detect it.
    let switch = observe_start_at(&path, "scope-a", "sess-2");
    assert_eq!(
        switch,
        Some(SwitchObserved { previous_session_id: "sess-1".to_owned(), previous_ended: false }),
        "marker mismatch at SessionStart must report the crash switch"
    );
    assert_eq!(read_active_at(&path, "scope-a"), Some(("sess-2".to_owned(), false)));
}

#[test]
fn clean_cycle_reports_prev_ended() {
    let (_tmp, path) = temp_sessions_file();
    assert_eq!(observe_start_at(&path, "scope-a", "sess-1"), None);
    observe_end_at(&path, "scope-a", "sess-1");
    assert_eq!(
        read_active_at(&path, "scope-a"),
        Some(("sess-1".to_owned(), true)),
        "SessionEnd must set ended=true while keeping the session id for handover"
    );
    let switch = observe_start_at(&path, "scope-a", "sess-2");
    assert_eq!(
        switch,
        Some(SwitchObserved { previous_session_id: "sess-1".to_owned(), previous_ended: true })
    );
}

#[test]
fn same_session_restart_is_not_a_switch() {
    let (_tmp, path) = temp_sessions_file();
    assert_eq!(observe_start_at(&path, "scope-a", "sess-1"), None);
    assert_eq!(
        observe_start_at(&path, "scope-a", "sess-1"),
        None,
        "re-observing the same session id must not report a switch"
    );
}

#[test]
fn scopes_are_independent() {
    let (_tmp, path) = temp_sessions_file();
    assert_eq!(observe_start_at(&path, "scope-a", "sess-1"), None);
    assert_eq!(
        observe_start_at(&path, "scope-b", "sess-9"),
        None,
        "a session in another scope must not look like a switch"
    );
    assert_eq!(read_active_at(&path, "scope-a"), Some(("sess-1".to_owned(), false)));
    assert_eq!(read_active_at(&path, "scope-b"), Some(("sess-9".to_owned(), false)));
}

#[test]
fn corrupt_file_treated_as_absent() {
    let (_tmp, path) = temp_sessions_file();
    std::fs::write(&path, b"{ not json !!!").expect("write garbage");
    // Must not panic; corrupt == no marker.
    let switch = observe_start_at(&path, "scope-a", "sess-1");
    assert_eq!(switch, None, "corrupt marker file must be treated as absent");
    // The next successful write replaces the corrupt file with a valid one.
    assert_eq!(read_active_at(&path, "scope-a"), Some(("sess-1".to_owned(), false)));
}

#[test]
fn stale_session_end_keeps_marker() {
    let (_tmp, path) = temp_sessions_file();
    assert_eq!(observe_start_at(&path, "scope-a", "sess-1"), None);
    assert!(observe_start_at(&path, "scope-a", "sess-2").is_some());
    // Out-of-order end for the OLD session must not touch the active marker.
    observe_end_at(&path, "scope-a", "sess-1");
    assert_eq!(
        read_active_at(&path, "scope-a"),
        Some(("sess-2".to_owned(), false)),
        "a stale SessionEnd must leave the active marker untouched"
    );
}

#[test]
fn session_ids_sanitized_to_scope_alphabet() {
    assert_eq!(sanitize_session_id("a:b/c d"), "a-b-c-d");
    assert_eq!(sanitize_session_id("sess_OK-1.x"), "sess_OK-1.x");
    let (_tmp, path) = temp_sessions_file();
    assert_eq!(observe_start_at(&path, "scope-a", "a:b/c"), None);
    assert_eq!(
        read_active_at(&path, "scope-a"),
        Some(("a-b-c".to_owned(), false)),
        "the marker must store the SANITIZED session id"
    );
}

#[test]
fn switch_meta_carries_both_fields() {
    let meta = switch_meta(&SwitchObserved {
        previous_session_id: "sess-1".to_owned(),
        previous_ended: true,
    });
    assert_eq!(
        meta,
        vec![
            ("switch_from".to_owned(), serde_json::Value::String("sess-1".to_owned())),
            ("switch_prev_ended".to_owned(), serde_json::Value::Bool(true)),
        ]
    );
}

// ── E2E tier: real binary, two consecutive sessions ──────────────────────────

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

async fn run_hook(envelope: &str, scopes: &PathBuf, sessions: &PathBuf) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_STORE_URL", "memory://")
        .env("LUNARIS_HOOK_SCOPE", "session-switch-e2e")
        .env("LUNARIS_SCOPES_FILE", scopes.to_str().unwrap())
        .env("LUNARIS_SESSIONS_FILE", sessions.to_str().unwrap())
        .env("LUNARIS_HOOK_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
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

#[tokio::test(flavor = "multi_thread")]
async fn e2e_switch_emits_stderr_line_and_exits_0() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes = tmp.path().join("scopes.json");
    let sessions = tmp.path().join("sessions.json");

    let out_a = run_hook(START_A, &scopes, &sessions).await;
    assert_eq!(out_a.status.code(), Some(0), "first SessionStart must exit 0");
    let stderr_a = String::from_utf8_lossy(&out_a.stderr);
    assert!(
        !stderr_a.contains("session_switch"),
        "first session must not report a switch, got: {stderr_a}"
    );

    let out_b = run_hook(START_B, &scopes, &sessions).await;
    assert_eq!(out_b.status.code(), Some(0), "second SessionStart must exit 0");
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("session_switch")
            && stderr_b.contains(r#""from":"sess-a""#)
            && stderr_b.contains(r#""to":"sess-b""#),
        "switch must emit the single-line JSON stderr signal, got: {stderr_b}"
    );
}
