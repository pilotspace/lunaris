use lunaris_test_harness::open_test_store;
use std::process::Stdio;
use std::time::Duration;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

const POST_TOOL_USE_JSON: &str = r#"{
  "hook_event_name": "PostToolUse",
  "session_id": "sess-abc123",
  "cwd": "/tmp/test-hook-repo",
  "tool_name": "Edit",
  "tool_input": {"path": "src/main.rs"},
  "tool_response": {"success": true, "output": "edited"},
  "event_id": "evt-002"
}"#;

#[tokio::test(flavor = "multi_thread")]
async fn post_tool_use_exits_0() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes_json = tmp.path().join("scopes.json");
    // 0.7.0 port off `memory://`: a harness-issued store URL (ephemeral
    // child-process Moon, else `memory://`). `store` owns the Moon child and
    // must outlive the spawned hook.
    let store = open_test_store().await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_STORE_URL", store.url())
        .env("LUNARIS_HOOK_SCOPE", "post-tool-use-test")
        .env("LUNARIS_SCOPES_FILE", scopes_json.to_str().unwrap())
        .env("LUNARIS_HOOK_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(POST_TOOL_USE_JSON.as_bytes()).await.unwrap();
    drop(stdin);
    let status =
        timeout(Duration::from_secs(10), child.wait()).await.expect("timeout").expect("wait");
    assert_eq!(status.code(), Some(0), "PostToolUse must exit 0");
}
