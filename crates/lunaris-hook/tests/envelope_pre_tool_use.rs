//! Integration test: PreToolUse envelope → exit 0.
//!
//! RED: binary exits 73 unconditionally, so this test FAILS. GREEN replaces
//! the stub main.rs with the full implementation.

use std::process::Stdio;
use std::time::Duration;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

const PRE_TOOL_USE_JSON: &str = r#"{
  "hook_event_name": "PreToolUse",
  "session_id": "sess-abc123",
  "transcript_path": "/tmp/transcript.jsonl",
  "cwd": "/tmp/test-hook-repo",
  "tool_name": "Edit",
  "tool_input": {"path": "src/main.rs", "old_str": "foo", "new_str": "bar"},
  "event_id": "evt-001"
}"#;

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_use_exits_0() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes_json = tmp.path().join("scopes.json");

    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_STORE_URL", "memory://")
        .env("LUNARIS_HOOK_SCOPE", "pre-tool-use-test")
        .env("LUNARIS_SCOPES_FILE", scopes_json.to_str().unwrap())
        .env("LUNARIS_HOOK_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn lunaris-hook");

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(PRE_TOOL_USE_JSON.as_bytes()).await.unwrap();
    drop(stdin);

    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("timeout waiting for lunaris-hook")
        .expect("wait failed");

    assert_eq!(status.code(), Some(0), "PreToolUse must exit 0");
}
