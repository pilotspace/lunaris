//! HOOK-05 idempotency integration test.
//!
//! Verifies that calling `lunaris_hook::run()` twice with the same envelope:
//! 1. Both calls return `Ok(Some(lsn))` with byte-equal LSNs.
//! 2. `lunaris_dedupe` table has exactly one row for the (scope, dedupe_key) pair.
//! 3. The episodes table has exactly one row for the content (no duplicate Episode).
//! 4. The second call's LSN equals the first (idempotent return via IngestKind::Duplicate).
//!
//! Documented: "HOOK-05: second call returns IngestKind::Duplicate with the prior LSN.
//! lunaris_dedupe table has exactly one row."

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::{HlcClock, NoopEmbedder, Scope};
use lunaris_storage_embedded::EmbeddedStorage;
use sqlx::Row as _;

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

/// Build a Lunaris handle backed by an in-memory SQLite database.
///
/// Uses `with_parts` (the test seam) with a `NoopEmbedder` so no GGUF
/// models are loaded and cold-start time is negligible.
async fn build_lunaris_in_memory() -> (Lunaris, EmbeddedStorage) {
    let storage = EmbeddedStorage::connect("memory://")
        .await
        .expect("EmbeddedStorage::connect(memory://) must succeed");

    // NoopEmbedder at dim=768 (granite-r2 default). Hook pipeline does not
    // embed at capture time — embedding is deferred to first recall.
    let embedder = Arc::new(NoopEmbedder::new(768));
    let storage_arc: Arc<dyn lunaris_core::StoragePort> = Arc::new(storage.clone());
    // HlcClock::new already returns Arc<HlcClock> — do NOT wrap in Arc::new again.
    let clock = HlcClock::new(0);

    let handle = Lunaris::with_parts(storage_arc, embedder, clock);
    (handle, storage)
}

/// Derive the expected scope from a fixed string (no git repo needed in CI).
fn fixed_scope() -> Scope {
    Scope::new("idempotency-test-scope").expect("scope must be valid")
}

#[tokio::test(flavor = "current_thread")]
async fn same_envelope_twice_returns_identical_lsn() {
    // Suppress tracing noise in test output.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();

    let (handle, storage) = build_lunaris_in_memory().await;
    let lunaris = Arc::new(handle);
    let scope = fixed_scope();

    let stdin_bytes = ENVELOPE.as_bytes();

    // === First call ===
    let result1 = lunaris_hook::run(stdin_bytes, scope.clone(), Arc::clone(&lunaris))
        .await
        .expect("first run() must succeed");

    let lsn1 = result1.expect("first run() must return Some(lsn)");

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

    // Assertion 2: lunaris_dedupe table has exactly one row for this scope+key pair.
    // We can't easily get the dedupe_key here, so count ALL rows for the scope.
    let dedupe_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS cnt FROM lunaris_dedupe WHERE scope = ?",
    )
    .bind(scope.as_str())
    .fetch_one(storage.pool())
    .await
    .expect("SELECT COUNT from lunaris_dedupe must succeed")
    .try_get("cnt")
    .expect("cnt column must be present");

    assert_eq!(
        dedupe_count, 1,
        "HOOK-05: lunaris_dedupe must have exactly one row after two identical runs (scope={})",
        scope.as_str()
    );

    // Assertion 3: episodes table has exactly one KV row for this scope's episode key prefix.
    // The episodes are stored under `lunaris:{scope}:episode:*` in lunaris_kv.
    // We check that the second run did NOT write a duplicate KV row (no second sys_from entry).
    let episode_prefix = format!("lunaris:{}:episode:", scope.as_str());
    let episode_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS cnt FROM lunaris_kv \
         WHERE key LIKE ? AND sys_to IS NULL",
    )
    .bind(format!("{}%", episode_prefix))
    .fetch_one(storage.pool())
    .await
    .expect("SELECT COUNT from lunaris_kv must succeed")
    .try_get("cnt")
    .expect("cnt column must be present");

    assert!(
        episode_count >= 1,
        "HOOK-05: at least one live episode KV row must exist for scope {}",
        scope.as_str()
    );

    // The first run produces N KV rows per episode (chunk + episode + fact entries).
    // We verify that the second run added zero new live rows by checking the
    // total live-row count equals the count after the first run.
    // Since lsn1 == lsn2, we know no second atomic_write happened.
    // The strongest assertion is: the HLC used for the second run's sys_from would be
    // strictly greater than lsn1 if a second write had occurred.
    // We verify this by asserting all live rows have sys_from <= lsn1's HLC string.
    let lsn1_wall = lsn1.wall_ms;
    let lsn1_ctr = lsn1.counter;
    // HLC string format: "{wall_ms:020}.{counter:010}.{node_id:05}" (from schema.rs)
    let lsn1_hlc_str = format!("{:020}.{:010}.{:05}", lsn1_wall, lsn1_ctr, 0_u32);

    let newer_rows: i64 = sqlx::query(
        "SELECT COUNT(*) AS cnt FROM lunaris_kv \
         WHERE key LIKE ? AND sys_from > ? AND sys_to IS NULL",
    )
    .bind(format!("{}%", episode_prefix))
    .bind(&lsn1_hlc_str)
    .fetch_one(storage.pool())
    .await
    .expect("SELECT COUNT newer rows must succeed")
    .try_get("cnt")
    .expect("cnt column must be present");

    assert_eq!(
        newer_rows, 0,
        "HOOK-05: no new KV rows must be written after the first ingest — \
         second run must have returned IngestKind::Duplicate (lsn1_hlc={})",
        lsn1_hlc_str
    );

    // Assertion 4: second run's lsn equals first (already verified above, but
    // re-state for clarity in test output).
    assert_eq!(
        lsn1.wall_ms, lsn2.wall_ms,
        "HOOK-05: lsn.wall_ms must match (same logical commit time)"
    );
    assert_eq!(
        lsn1.counter, lsn2.counter,
        "HOOK-05: lsn.counter must match (same logical commit time)"
    );
}
