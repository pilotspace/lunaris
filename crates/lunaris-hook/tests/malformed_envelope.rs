//! Malformed envelopes exit 64 with structured stderr JSON (when LOG_JSON=1).

use lunaris_test_harness::open_test_store;
use std::process::Stdio;
use std::time::Duration;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

#[tokio::test(flavor = "multi_thread")]
async fn invalid_json_exits_64() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes_json = tmp.path().join("scopes.json");
    // 0.7.0 port off `memory://`: a harness-issued store URL (ephemeral
    // child-process Moon, else `memory://`). `store` owns the Moon child and
    // must outlive the spawned hook.
    let store = open_test_store().await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_STORE_URL", store.url())
        // Socket-first routing outranks the store named above: a reachable
        // contextd would serve this from ITS store, silently. Force the direct leg.
        .env("LUNARIS_CONTEXTD_SOCKET", "/nonexistent/lunaris-test.sock")
        .env("LUNARIS_HOOK_SCOPE", "malformed-test")
        .env("LUNARIS_SCOPES_FILE", scopes_json.to_str().unwrap())
        .env("LUNARIS_HOOK_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"{not valid json}").await.unwrap();
    drop(stdin);
    let status =
        timeout(Duration::from_secs(10), child.wait()).await.expect("timeout").expect("wait");
    assert_eq!(status.code(), Some(64), "Invalid JSON must exit 64");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_hook_event_name_exits_64() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scopes_json = tmp.path().join("scopes.json");
    // 0.7.0 port off `memory://`: a harness-issued store URL (ephemeral
    // child-process Moon, else `memory://`). `store` owns the Moon child and
    // must outlive the spawned hook.
    let store = open_test_store().await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_lunaris-hook"))
        .env("LUNARIS_STORE_URL", store.url())
        // Socket-first routing outranks the store named above: a reachable
        // contextd would serve this from ITS store, silently. Force the direct leg.
        .env("LUNARIS_CONTEXTD_SOCKET", "/nonexistent/lunaris-test.sock")
        .env("LUNARIS_HOOK_SCOPE", "malformed-test-2")
        .env("LUNARIS_SCOPES_FILE", scopes_json.to_str().unwrap())
        .env("LUNARIS_HOOK_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(br#"{"session_id": "s1"}"#).await.unwrap();
    drop(stdin);
    let status =
        timeout(Duration::from_secs(10), child.wait()).await.expect("timeout").expect("wait");
    assert_eq!(status.code(), Some(64), "Missing hook_event_name must exit 64");
}
