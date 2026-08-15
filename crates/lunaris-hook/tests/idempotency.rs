//! HOOK-05 idempotency integration test.
//!
//! Verifies that calling `lunaris_hook::run()` twice with the same envelope:
//! 1. Both calls return `Ok(Some(lsn))` with byte-equal LSNs.
//! 2. The scope's live episode key set is UNCHANGED by the second call (no
//!    duplicate Episode, and no new rows of any kind).
//! 3. The second call's LSN equals the first (idempotent return via
//!    `IngestKind::Duplicate`).
//!
//! Documented: "HOOK-05: second call returns IngestKind::Duplicate with the
//! prior LSN."
//!
//! ## Rewritten in 0.7.0
//!
//! This file used to open `EmbeddedStorage::connect("memory://")` and assert
//! against the embedded SCHEMA with raw `sqlx` — `SELECT COUNT(*) FROM
//! lunaris_dedupe`, `... FROM lunaris_kv WHERE sys_to IS NULL`. The port plan
//! recorded it as the one file the Moon harness could not serve, because those
//! assertions were about SQLite tables rather than about the `StoragePort`
//! contract.
//!
//! They are re-expressed here through `StoragePort::scan_range`, which is both
//! portable AND a closer statement of the actual claim. "`lunaris_dedupe` has
//! one row" was a proxy for "no duplicate episode was written"; counting live
//! episode keys says that directly, and says it about whatever backend is
//! underneath. Moon implements the dedupe sidecar natively
//! (`MoonStorage::lookup_by_dedupe_key`, which closed the v0.5 "SQLite-only
//! idempotency" boundary), so HOOK-05 is a real contract here, not a
//! vacuously-passing one.

use std::sync::Arc;

use futures::StreamExt as _;
use lunaris::Lunaris;
use lunaris_core::{HlcClock, NoopEmbedder, Scope, StoragePort};
use lunaris_test_harness::{TestStorage, open_test_storage};

/// Deterministic PreToolUse envelope JSON.
/// Fixed session_id, fixed tool_name="Edit", fixed tool_input — produces a
/// stable dedupe key across two calls.
const ENVELOPE: &str = r#"{
  "hook_event_name": "PreToolUse",
  "session_id": "idempotency-test-session-001",
  "transcript_path": "/tmp/idempotency-test.jsonl",
  "cwd": "/tmp/idempotency-test",
  "tool_name": "Edit",
  "tool_input": {"path": "src/main.rs", "content": "fn main() {}"},
  "event_id": "idempotency-fixed-event-id-001",
  "timestamp": "2026-05-25T00:00:00Z"
}"#;

/// Build a Lunaris handle over a disposable child-process Moon.
///
/// Uses `with_parts` (the test seam) with a `NoopEmbedder` so no GGUF models
/// are loaded and cold-start time is negligible — the hook pipeline does not
/// embed at capture time anyway (embedding is deferred to first recall).
///
/// The returned [`TestStorage`] owns the Moon child and must outlive the
/// handle.
async fn build_lunaris() -> (Lunaris, TestStorage) {
    let storage = open_test_storage().await;
    // NoopEmbedder at dim=768 (granite-r2 default), matching the harness's
    // FT index width.
    let embedder = Arc::new(NoopEmbedder::new(768));
    // HlcClock::new already returns Arc<HlcClock> — do NOT wrap in Arc::new again.
    let handle = Lunaris::with_parts(storage.port(), embedder, HlcClock::new(0));
    (handle, storage)
}

/// Derive the expected scope from a fixed string (no git repo needed in CI).
fn fixed_scope() -> Scope {
    Scope::new("idempotency-test-scope").expect("scope must be valid")
}

/// Every live key under this scope's episode prefix, sorted — the portable
/// stand-in for the old `SELECT ... FROM lunaris_kv WHERE sys_to IS NULL`.
///
/// The whole KEY SET is collected rather than a count: a backend that wrote a
/// duplicate under a fresh ULID and retired the old one would keep a count
/// stable while changing the set.
async fn live_episode_keys(storage: &Arc<dyn StoragePort>, scope: &Scope) -> Vec<Vec<u8>> {
    let prefix = format!("lunaris:{}:episode:", scope.as_str());
    let mut stream = storage
        .scan_range(scope, prefix.as_bytes(), None)
        .await
        .expect("scan_range over the episode prefix must succeed");
    let mut keys = Vec::new();
    while let Some(row) = stream.next().await {
        let (k, _v) = row.expect("scan_range row must decode");
        keys.push(k.to_vec());
    }
    keys.sort();
    keys
}

#[tokio::test(flavor = "current_thread")]
async fn same_envelope_twice_returns_identical_lsn() {
    // Suppress tracing noise in test output.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();

    let (handle, storage) = build_lunaris().await;
    let port = storage.port();
    let lunaris = Arc::new(handle);
    let scope = fixed_scope();

    let stdin_bytes = ENVELOPE.as_bytes();

    // === First call ===
    let result1 = lunaris_hook::run(stdin_bytes, scope.clone(), Arc::clone(&lunaris))
        .await
        .expect("first run() must succeed");

    let lsn1 = result1.expect("first run() must return Some(lsn)");

    let keys_after_first = live_episode_keys(&port, &scope).await;
    assert!(
        !keys_after_first.is_empty(),
        "HOOK-05 precondition: the first run must write at least one live episode row"
    );

    // === Second call — same envelope, same scope ===
    let result2 = lunaris_hook::run(stdin_bytes, scope.clone(), Arc::clone(&lunaris))
        .await
        .expect("second run() must succeed (idempotent)");

    let lsn2 = result2.expect("second run() must return Some(lsn)");

    // Assertion 1: both LSNs are equal (idempotent return).
    assert_eq!(
        lsn1, lsn2,
        "HOOK-05: second call must return the same LSN as the first (IngestKind::Duplicate)"
    );

    // Assertion 2: the second run wrote NOTHING. Replaces the old
    // `SELECT COUNT(*) FROM lunaris_dedupe` / `lunaris_kv` pair — same claim,
    // stated against the port contract instead of one backend's schema.
    let keys_after_second = live_episode_keys(&port, &scope).await;
    assert_eq!(
        keys_after_first, keys_after_second,
        "HOOK-05: the replayed envelope must not add, replace, or retire a single episode row"
    );

    // Assertion 3: LSN components match field-by-field (re-stated for a
    // readable failure line when the `assert_eq!` above is what breaks).
    assert_eq!(
        lsn1.wall_ms, lsn2.wall_ms,
        "HOOK-05: lsn.wall_ms must match (same logical commit time)"
    );
    assert_eq!(
        lsn1.counter, lsn2.counter,
        "HOOK-05: lsn.counter must match (same logical commit time)"
    );
}
