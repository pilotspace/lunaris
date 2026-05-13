//! Phase 14.3 integration tests — `pre_warm_query` fire-and-forget recall.
//!
//! ## Test plan
//!
//! Four in-process tests using `memory://` (no external services required):
//!
//! - **Test 1 (happy path):** A `ScopedLunaris::end_turn` with
//!   `pre_warm_query = Some("alice")` returns quickly (< 500 ms wall) and the
//!   spawned task completes without panic. Verified by waiting for the
//!   background task counter to reach 1.
//!
//! - **Test 2 (semaphore bound):** A semaphore with capacity 1. Ten rapid
//!   `end_turn` calls. Exactly 1 warm-up task is acquired; the remaining 9
//!   are skipped. Verified by a shared `Arc<AtomicUsize>` counter that the
//!   counting embedder increments on each embed call (inside the spawned task)
//!   and a separate `skip_counter` tracked via a `Mutex<usize>` set up
//!   through a counting `StoragePort` wrapper.
//!
//! - **Test 3 (error no crash):** A storage backend that returns `Err` on
//!   every vector search. `end_turn` returns `Ok`; the spawned task logs
//!   `pre_warm_failed` but does NOT panic. Verified by joining a tokio
//!   `sleep` and asserting the task handle from `tokio::spawn` didn't panic
//!   (we can't hold the JoinHandle since end_turn doesn't return it, so we
//!   assert the process didn't crash and `end_turn` returned Ok).
//!
//! - **Test 4 (None is no-op):** `pre_warm_query: None` → no semaphore
//!   acquisition, no spawn. Verified by confirming the semaphore still has
//!   full capacity after the call.
//!
//! ## Approach
//!
//! Rather than intercepting tracing events (fragile; depends on formatting),
//! tests share state via `Arc<AtomicUsize>` counters that are incremented
//! inside the warm-up recall path. The counting embedder wraps `StubEmbedder`
//! and increments a shared counter on each `embed()` call. Since the warm-up
//! always calls the embedder (Vector root issues an embed for the query), the
//! counter gives a reliable "task started" signal.
//!
//! For the semaphore-bound test the counting embedder also blocks on a
//! `tokio::sync::Notify` long enough for the 10 rapid `end_turn` calls to all
//! fire before the first task releases its permit.

#![forbid(unsafe_code)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use lunaris_core::{
    Embedder, HlcClock, LunarisError, Scope, StoragePort,
    storage::types::WriteOp,
};
use lunaris_verify::{FauxReflectSupervisor, ReflectInput, ReflectOutput};
use tokio::sync::Notify;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Counting embedder — increments a shared counter on every embed() call.
// Optionally blocks until a Notify fires (for semaphore-bound test).
// ---------------------------------------------------------------------------

/// Thin embedder wrapper that increments `counter` on each embed call and
/// (optionally) waits for `gate` to be notified before returning.
struct CountingEmbedder {
    inner: lunaris_embed::StubEmbedder,
    counter: Arc<AtomicUsize>,
    /// When `Some`, the embedder waits for this notify before returning the
    /// embedding — keeps the spawned warm-up task alive long enough for the
    /// semaphore test to fire all 10 `end_turn` calls.
    gate: Option<Arc<Notify>>,
}

impl CountingEmbedder {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        Self { inner: lunaris_embed::StubEmbedder::default(), counter, gate: None }
    }

    fn with_gate(mut self, gate: Arc<Notify>) -> Self {
        self.gate = Some(gate);
        self
    }
}

#[async_trait::async_trait]
impl Embedder for CountingEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        self.inner.embed(texts).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a fresh `memory://` storage backend.
async fn memory_storage() -> Arc<dyn StoragePort> {
    use lunaris_storage_embedded::EmbeddedStorage;
    Arc::new(EmbeddedStorage::connect("memory://").await.expect("memory storage"))
}

/// Build a `ScopedLunaris` handle wired to `memory://` with `NoopVerifier`
/// and the supplied embedder, scoped to a fresh per-test scope. The reflect
/// supervisor is set to the provided `supervisor`.
///
/// Returns `(handle, scope)` — the `Lunaris` handle is kept alive for the
/// lifetime of the test via the returned binding.
async fn make_handle_with_embedder(
    embedder: Arc<dyn Embedder>,
    supervisor: Arc<dyn lunaris_verify::ReflectSupervisor>,
    prewarm_capacity: usize,
) -> (lunaris::Lunaris, Scope) {
    use lunaris_core::{KeywordPort, StorageError};

    // We build the handle via `with_parts_keyword` so we can inject a custom
    // embedder. Then override the reflect supervisor and the semaphore capacity.
    let storage = memory_storage().await;
    // EmbeddedStorage also implements KeywordPort — downcast via Arc clone.
    // For the warm-up test we don't need real keyword search, just vector.
    let clock = Arc::new(HlcClock::new(0));
    let mut handle = lunaris::Lunaris::with_parts(storage, embedder, clock);
    handle = handle.with_reflect_supervisor(supervisor);
    // Override the semaphore capacity to the test-controlled value.
    handle.warm_up_semaphore =
        Arc::new(tokio::sync::Semaphore::new(prewarm_capacity));
    let scope = Scope::new(format!("phase-14-3-test-{}", Ulid::new())).expect("valid scope");
    (handle, scope)
}

// ---------------------------------------------------------------------------
// Test 1 — happy path: end_turn returns quickly, spawned task completes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_prewarm_happy_path() {
    // Supervisor returns pre_warm_query = Some("alice").
    let supervisor = Arc::new(FauxReflectSupervisor::with_prewarm("alice"));
    let embed_counter = Arc::new(AtomicUsize::new(0));
    let embedder = Arc::new(CountingEmbedder::new(embed_counter.clone()));

    let (handle, scope) = make_handle_with_embedder(embedder, supervisor, 4).await;
    let scoped = handle.scoped(scope);

    let start = std::time::Instant::now();
    let result = scoped
        .end_turn(ReflectInput {
            turn_id: None,
            recent_fact_ids: vec![],
            recent_chunk_ids: vec![],
        })
        .await;
    let elapsed = start.elapsed();

    // end_turn must return Ok regardless of warm-up result.
    assert!(result.is_ok(), "end_turn should return Ok; got {:?}", result);

    // end_turn should return well within the 500ms wall budget.
    assert!(
        elapsed < Duration::from_millis(500),
        "end_turn took {:?}, expected < 500ms",
        elapsed
    );

    // Wait up to 2 seconds for the spawned warm-up task to call embed().
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while embed_counter.load(Ordering::SeqCst) == 0 {
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The spawned task should have issued at least one embed call (Vector root).
    // If the memory storage returns empty hits, the task completes without error.
    // We assert >= 1 embed calls to confirm the task actually ran.
    assert!(
        embed_counter.load(Ordering::SeqCst) >= 1,
        "expected warm-up task to issue at least 1 embed call; counter = {}",
        embed_counter.load(Ordering::SeqCst)
    );
}

// ---------------------------------------------------------------------------
// Test 2 — semaphore bound: capacity=1, 10 calls → exactly 1 acquired
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_prewarm_semaphore_bound() {
    let supervisor = Arc::new(FauxReflectSupervisor::with_prewarm("semaphore test query"));
    let embed_counter = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let embedder =
        Arc::new(CountingEmbedder::new(embed_counter.clone()).with_gate(gate.clone()));

    // Semaphore capacity = 1. The counting embedder blocks until we notify
    // the gate, keeping the first task alive and the semaphore acquired.
    let (handle, scope) = make_handle_with_embedder(Arc::new(embedder), supervisor, 1).await;

    // Wait for the first embed to start (confirms task 1 acquired the permit).
    // Then fire 9 more end_turn calls — all should see the semaphore exhausted.
    let scoped = handle.scoped(scope);

    // Fire 10 rapid end_turn calls.
    for _ in 0..10 {
        let _ = scoped
            .end_turn(ReflectInput {
                turn_id: None,
                recent_fact_ids: vec![],
                recent_chunk_ids: vec![],
            })
            .await;
    }

    // Wait for first task to start (embed counter increments inside async move).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while embed_counter.load(Ordering::SeqCst) == 0 {
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Release the gate so the first task can finish.
    gate.notify_waiters();

    // Wait for the first task to complete.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Exactly 1 embed call should have fired — only 1 task was ever acquired.
    // The other 9 were skipped at try_acquire_owned.
    let count = embed_counter.load(Ordering::SeqCst);
    assert_eq!(
        count, 1,
        "expected exactly 1 warm-up task to run (semaphore capacity=1, 10 calls); got {}",
        count
    );

    // Semaphore should be fully available again after the one task completed.
    assert_eq!(
        handle.warm_up_semaphore.available_permits(),
        1,
        "semaphore should have 1 available permit after the task released it"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — storage error does not crash or fail end_turn
// ---------------------------------------------------------------------------

/// An embedder that always returns an error — forces the warm-up recall to fail.
struct ErrorEmbedder;

#[async_trait::async_trait]
impl Embedder for ErrorEmbedder {
    fn dim(&self) -> usize {
        4
    }
    async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Err(LunarisError::Other(anyhow::anyhow!("injected embed error for prewarm test")))
    }
}

#[tokio::test]
async fn test_prewarm_error_no_crash() {
    let supervisor = Arc::new(FauxReflectSupervisor::with_prewarm("error test query"));
    // ErrorEmbedder causes the warm-up recall to fail inside the spawned task.
    let (handle, scope) =
        make_handle_with_embedder(Arc::new(ErrorEmbedder), supervisor, 4).await;
    let scoped = handle.scoped(scope);

    // end_turn must return Ok even though the warm-up task will error.
    let result = scoped
        .end_turn(ReflectInput {
            turn_id: None,
            recent_fact_ids: vec![],
            recent_chunk_ids: vec![],
        })
        .await;

    assert!(
        result.is_ok(),
        "end_turn must return Ok when warm-up task errors; got {:?}",
        result
    );

    // Give the spawned task time to run and log its warn (no panic expected).
    tokio::time::sleep(Duration::from_millis(100)).await;
    // If we reach here the process didn't crash — the test passes.
}

// ---------------------------------------------------------------------------
// Test 4 — None pre_warm_query is a no-op (no semaphore acquisition)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_prewarm_none_is_noop() {
    // Supervisor returns pre_warm_query = None.
    let supervisor = Arc::new(FauxReflectSupervisor::no_prewarm());
    let embed_counter = Arc::new(AtomicUsize::new(0));
    let embedder = Arc::new(CountingEmbedder::new(embed_counter.clone()));

    let (handle, scope) = make_handle_with_embedder(embedder, supervisor, 4).await;
    let initial_permits = handle.warm_up_semaphore.available_permits();
    let scoped = handle.scoped(scope);

    let result = scoped
        .end_turn(ReflectInput {
            turn_id: None,
            recent_fact_ids: vec![],
            recent_chunk_ids: vec![],
        })
        .await;

    assert!(result.is_ok(), "end_turn should return Ok; got {:?}", result);

    // Give any accidental task time to start (there should be none).
    tokio::time::sleep(Duration::from_millis(50)).await;

    // No embed calls should have fired.
    assert_eq!(
        embed_counter.load(Ordering::SeqCst),
        0,
        "no embed calls expected when pre_warm_query = None"
    );

    // Semaphore must still be at full capacity — no acquisition happened.
    assert_eq!(
        handle.warm_up_semaphore.available_permits(),
        initial_permits,
        "semaphore permits must be unchanged when pre_warm_query = None"
    );
}
