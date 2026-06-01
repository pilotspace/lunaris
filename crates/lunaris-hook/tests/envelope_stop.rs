use std::process::Stdio;
use std::time::Duration;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

const STOP_JSON: &str = r#"{
  "hook_event_name": "Stop",
  "session_id": "sess-abc123",
  "cwd": "/tmp/test-hook-repo",
  "event_id": "evt-003"
}"#;

#[tokio::test(flavor = "multi_thread")]
async fn stop_exits_0() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes_json = tmp.path().join("scopes.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_STORE_URL", "memory://")
        .env("LUNARIS_HOOK_SCOPE", "stop-test")
        .env("LUNARIS_SCOPES_FILE", scopes_json.to_str().unwrap())
        .env("LUNARIS_HOOK_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(STOP_JSON.as_bytes()).await.unwrap();
    drop(stdin);
    let status =
        timeout(Duration::from_secs(10), child.wait()).await.expect("timeout").expect("wait");
    assert_eq!(status.code(), Some(0), "Stop must exit 0");
}
