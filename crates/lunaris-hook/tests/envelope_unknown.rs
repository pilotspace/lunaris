//! Unknown event kind exits 0 (intentional forward-compat no-op, NOT an error).
//!
//! B2 fix: ROADMAP SC#2 requires exit 0 for unknown kinds. Exit 66 is
//! reserved for Phase 24 filter-rejected events.

use lunaris_test_harness::open_test_store;
use std::process::Stdio;
use std::time::Duration;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

const UNKNOWN_JSON: &str = r#"{
  "hook_event_name": "UnknownFutureEvent",
  "session_id": "sess-abc123",
  "cwd": "/tmp/test-hook-repo"
}"#;

#[tokio::test(flavor = "multi_thread")]
async fn unknown_event_kind_exits_0() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes_json = tmp.path().join("scopes.json");
    // 0.7.0 port off `memory://`: a harness-issued store URL (ephemeral
    // child-process Moon, else `memory://`). `store` owns the Moon child and
    // must outlive the spawned hook.
    let store = open_test_store().await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_STORE_URL", store.url())
        .env("LUNARIS_HOOK_SCOPE", "unknown-test")
        .env("LUNARIS_SCOPES_FILE", scopes_json.to_str().unwrap())
        .env("LUNARIS_HOOK_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(UNKNOWN_JSON.as_bytes()).await.unwrap();
    drop(stdin);
    let status =
        timeout(Duration::from_secs(10), child.wait()).await.expect("timeout").expect("wait");
    assert_eq!(
        status.code(),
        Some(0),
        "Unknown event kind must exit 0 (forward-compat no-op per ROADMAP SC#2 + B2 fix; \
         exit 66 is Phase 24 reserved for filter-rejected events)"
    );
}
