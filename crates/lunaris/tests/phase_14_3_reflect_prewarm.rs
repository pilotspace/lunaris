//! Phase 14.3 integration smoke tests — `ReflectOutput.pre_warm_query` → fire-and-forget recall.
//!
//! ## What is pinned
//!
//! Four sub-tests prove the composition introduced in feat commit 5129adf:
//!
//! 1. **`prewarm_happy_path_spawns_warm_up`** — supervisor returns
//!    `pre_warm_query: Some("alice")`. `end_turn` returns quickly (< 500ms wall).
//!    After a brief sleep, the semaphore is back to full available permits,
//!    confirming the spawned task ran and released the permit.
//!
//! 2. **`prewarm_none_is_noop`** — supervisor returns `pre_warm_query: None`.
//!    Semaphore available-permits count is unchanged after `end_turn`. No spawn.
//!
//! 3. **`prewarm_semaphore_bound_is_respected`** — `with_prewarm_concurrency(1)`.
//!    Manually hold the single permit via `try_acquire_owned`. Call `end_turn`
//!    with `pre_warm_query: Some(...)`. `end_turn` returns Ok without blocking
//!    (semaphore full → skip path). Drop the held permit — permits return to 1.
//!
//! 4. **`prewarm_storage_error_does_not_crash_agent`** — embedder that always
//!    returns `Err` forces the spawned warm-up recall to fail. `end_turn` still
//!    returns `Ok`; the agent turn is unaffected.
//!
//! ## Storage substrate
//!
//! A `lunaris-test-harness` fixture (0.7.0 port off `memory://`): an ephemeral
//! child-process Moon, degrading to the embedded backend where no Moon binary
//! resolves. All four tests reach it via the `make_handle` helper below, which
//! sizes the store from `embedder.dim()` — these fixtures are 4-d, and Moon's
//! FT index width is fixed at `FT.CREATE` and never resized.
//!
//! ## No `FauxReflectSupervisor`
//!
//! The name does not exist in any production crate. An inline
//! `StubReflectSupervisor` (mirroring the Phase 14.2 pattern) is defined in
//! this file and used directly.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris::Lunaris;
use lunaris_core::{Embedder, HlcClock, LunarisError, Scope, StubEmbedder};
use lunaris_test_harness::{Policy, TestStorage, open_test_storage_with};
use lunaris_verify::{ReflectInput, ReflectOutput, ReflectSupervisor};

// ---------------------------------------------------------------------------
// StubReflectSupervisor — returns a fixed ReflectOutput per call.
// ---------------------------------------------------------------------------

/// Synchronously returns the canned `ReflectOutput` supplied at construction.
/// Mirrors the `StubReflectSupervisor` in `phase_14_2_reflect_boost.rs` —
/// no shared mutable state needed here since each test constructs its own.
#[derive(Clone)]
struct StubReflectSupervisor {
    output: ReflectOutput,
}

#[async_trait]
impl ReflectSupervisor for StubReflectSupervisor {
    async fn reflect(&self, _input: ReflectInput) -> Result<ReflectOutput, LunarisError> {
        Ok(self.output.clone())
    }
}

// ---------------------------------------------------------------------------
// ErrorEmbedder — forces the spawned warm-up recall to fail.
// ---------------------------------------------------------------------------

/// An embedder that always returns `Err`. When wired as the handle's embedder,
/// the warm-up recall spawned by `end_turn` will hit `pre_warm_failed` log path.
/// This lets test 4 confirm the agent turn is unaffected by warm-up errors.
struct ErrorEmbedder;

#[async_trait]
impl Embedder for ErrorEmbedder {
    fn dim(&self) -> usize {
        4
    }

    async fn embed_batch(&self, _inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Err(LunarisError::Retrieve(lunaris_core::RetrieveError::Backend(
            "injected embed error for phase-14.3 prewarm test".to_string(),
        )))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `Lunaris` handle wired to a harness-issued storage backend and the
/// supplied embedder + reflect supervisor. The semaphore capacity is set via
/// `with_prewarm_concurrency`.
///
/// The store is sized from `embedder.dim()` — every fixture here is 4-d, and
/// Moon fixes its FT index width at `FT.CREATE` without resizing. The returned
/// [`TestStorage`] guard owns the Moon child and must outlive the handle.
async fn make_handle(
    embedder: Arc<dyn Embedder>,
    supervisor: Arc<dyn ReflectSupervisor>,
    prewarm_capacity: usize,
) -> (Lunaris, TestStorage) {
    let storage = open_test_storage_with(Policy::from_env(), embedder.dim()).await;
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts(storage.port(), embedder, clock)
        .with_reflect_supervisor(supervisor)
        .with_prewarm_concurrency(prewarm_capacity);
    (handle, storage)
}

/// Convenience: build a handle with the noop-fast `StubEmbedder` (4-d).
async fn make_stub_handle(
    supervisor: Arc<dyn ReflectSupervisor>,
    prewarm_capacity: usize,
) -> (Lunaris, TestStorage) {
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(4));
    make_handle(embedder, supervisor, prewarm_capacity).await
}

/// Build a `ReflectInput` with sensible defaults for the prewarm tests.
/// All four fields are populated; `turn_summary` is required (no Default fill).
fn make_input(summary: &str) -> ReflectInput {
    ReflectInput {
        turn_id: None,
        turn_summary: summary.to_string(),
        recent_fact_ids: vec![],
        recent_chunk_ids: vec![],
    }
}

// ---------------------------------------------------------------------------
// Test 1 — prewarm_happy_path_spawns_warm_up
// ---------------------------------------------------------------------------

/// Happy path: supervisor returns `pre_warm_query: Some("alice")`.
///
/// Assertions:
/// - `end_turn` returns `Ok`.
/// - `end_turn` wall-clock < 500ms (non-blocking fire-and-forget).
/// - After a brief sleep, the semaphore is back to full permits, confirming
///   the spawned task ran and released its permit.
#[tokio::test]
async fn prewarm_happy_path_spawns_warm_up() {
    let supervisor = Arc::new(StubReflectSupervisor {
        output: ReflectOutput {
            pre_warm_query: Some("alice".to_string()),
            ..ReflectOutput::default()
        },
    });

    let (handle, _storage) = make_stub_handle(supervisor, 4).await;
    let scope = Scope::new("test.prewarm-happy").unwrap();
    let initial_permits = handle.warm_up_semaphore().available_permits();
    let scoped = handle.scoped(scope);

    let start = std::time::Instant::now();
    let result = scoped.end_turn(make_input("happy path prewarm")).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "end_turn must return Ok; got {:?}", result.err());
    assert_eq!(
        result.unwrap().pre_warm_query,
        Some("alice".to_string()),
        "ReflectOutput.pre_warm_query must round-trip"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "end_turn must return in < 500ms (fire-and-forget); took {elapsed:?}"
    );

    // Give the spawned task ~200ms to run the recall and release the permit.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Semaphore must be back to full capacity — the spawned task released its permit.
    assert_eq!(
        handle.warm_up_semaphore().available_permits(),
        initial_permits,
        "warm_up_semaphore must be back to full ({initial_permits}) after task completes"
    );

    eprintln!("prewarm_happy_path_spawns_warm_up: PASS (elapsed={elapsed:?})");
}

// ---------------------------------------------------------------------------
// Test 2 — prewarm_none_is_noop
// ---------------------------------------------------------------------------

/// `pre_warm_query: None` → no spawn, semaphore permits unchanged.
#[tokio::test]
async fn prewarm_none_is_noop() {
    let supervisor = Arc::new(StubReflectSupervisor {
        output: ReflectOutput { pre_warm_query: None, ..ReflectOutput::default() },
    });

    let (handle, _storage) = make_stub_handle(supervisor, 4).await;
    let scope = Scope::new("test.prewarm-noop").unwrap();
    let initial_permits = handle.warm_up_semaphore().available_permits();
    let scoped = handle.scoped(scope);

    let result = scoped.end_turn(make_input("none prewarm")).await;
    assert!(result.is_ok(), "end_turn must return Ok; got {:?}", result.err());

    // Wait briefly; no spawn should have occurred.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Permits must be unchanged — no semaphore acquisition happened.
    assert_eq!(
        handle.warm_up_semaphore().available_permits(),
        initial_permits,
        "semaphore permits must be unchanged when pre_warm_query = None"
    );

    eprintln!("prewarm_none_is_noop: PASS");
}

// ---------------------------------------------------------------------------
// Test 3 — prewarm_semaphore_bound_is_respected
// ---------------------------------------------------------------------------

/// Semaphore capacity=1, manually hold the single permit, then fire `end_turn`
/// with `pre_warm_query: Some(...)`.
///
/// Assertions:
/// - `end_turn` returns `Ok` without blocking (semaphore full → skip path).
/// - Semaphore still shows 0 available permits while we hold the manual permit.
/// - After releasing the manual permit, available_permits returns to 1.
#[tokio::test]
async fn prewarm_semaphore_bound_is_respected() {
    let supervisor = Arc::new(StubReflectSupervisor {
        output: ReflectOutput {
            pre_warm_query: Some("semaphore test query".to_string()),
            ..ReflectOutput::default()
        },
    });

    let (handle, _storage) = make_stub_handle(supervisor, 1).await;
    let scope = Scope::new("test.prewarm-sembound").unwrap();

    // Manually hold the single permit — semaphore is now exhausted.
    let permit = handle
        .warm_up_semaphore()
        .try_acquire_owned()
        .expect("semaphore must have 1 permit available before test");
    assert_eq!(
        handle.warm_up_semaphore().available_permits(),
        0,
        "semaphore must show 0 after manual acquisition"
    );

    let scoped = handle.scoped(scope);

    // end_turn must return Ok even though the semaphore is full.
    // The skip path (try_acquire_owned returns Err) must NOT block.
    let start = std::time::Instant::now();
    let result = scoped.end_turn(make_input("semaphore bound test")).await;
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "end_turn must return Ok when semaphore is full; got {:?}",
        result.err()
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "end_turn must not block when semaphore is exhausted; took {elapsed:?}"
    );

    // Semaphore still at 0 — our manual permit is still held, and the spawn
    // was skipped (no second acquisition occurred).
    assert_eq!(
        handle.warm_up_semaphore().available_permits(),
        0,
        "semaphore must still be 0 (manual permit held, spawn was skipped)"
    );

    // Release the manual permit — permits must return to 1.
    drop(permit);
    assert_eq!(
        handle.warm_up_semaphore().available_permits(),
        1,
        "semaphore must return to 1 after manual permit is dropped"
    );

    eprintln!("prewarm_semaphore_bound_is_respected: PASS");
}

// ---------------------------------------------------------------------------
// Test 4 — prewarm_storage_error_does_not_crash_agent
// ---------------------------------------------------------------------------

/// `ErrorEmbedder` causes the spawned warm-up recall to fail (embed returns Err).
/// `end_turn` must still return `Ok`; the agent turn is completely unaffected.
/// After the task completes (and logs `pre_warm_failed`), the permit is released.
#[tokio::test]
async fn prewarm_storage_error_does_not_crash_agent() {
    let supervisor = Arc::new(StubReflectSupervisor {
        output: ReflectOutput {
            pre_warm_query: Some("error test query".to_string()),
            ..ReflectOutput::default()
        },
    });

    let embedder: Arc<dyn Embedder> = Arc::new(ErrorEmbedder);
    let (handle, _storage) = make_handle(embedder, supervisor, 4).await;
    let scope = Scope::new("test.prewarm-error").unwrap();
    let initial_permits = handle.warm_up_semaphore().available_permits();
    let scoped = handle.scoped(scope);

    // end_turn must return Ok even though the spawned warm-up will error.
    let result = scoped.end_turn(make_input("error path prewarm")).await;
    assert!(
        result.is_ok(),
        "end_turn must return Ok when warm-up task will error; got {:?}",
        result.err()
    );

    // Give the spawned task time to run, fail, log warn, and release the permit.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Permit must have been released even on error — semaphore back to full.
    assert_eq!(
        handle.warm_up_semaphore().available_permits(),
        initial_permits,
        "semaphore must return to full ({initial_permits}) even after warm-up error"
    );

    eprintln!("prewarm_storage_error_does_not_crash_agent: PASS");
}
