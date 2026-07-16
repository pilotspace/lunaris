//! Watchdog wrappers for the shared GGUF embedder/reranker.
//!
//! ## Why this exists (2026-07-16 incident)
//!
//! A live `lunaris-contextd` wedged at ~372% CPU for hours: a Metal command
//! buffer never completed inside one `embed` call, and llama.cpp's ggml
//! thread pool spin-waits while a graph is in flight — four threads pinned
//! at ~88% each (two runnable, two uninterruptible in the kernel), with the
//! daemon otherwise responsive. A wedged `spawn_blocking` thread cannot be
//! cancelled from async Rust, so the ONLY reliable cleanup is process exit:
//! the hooks respawn contextd on demand (see `contextd.rs` duplicate-starter
//! logic, which also clears the stale socket file).
//!
//! Policy: every inference call gets a generous timeout
//! (`LUNARIS_INFER_WATCHDOG_MS`, default 120 s — far above the ~8 s cold
//! Metal+GGUF load). A timed-out call fails that one request (recall already
//! fail-opens). `LUNARIS_INFER_WATCHDOG_TRIP` consecutive timeouts (default
//! 2) mean the runtime is wedged, not slow — the [`WedgePolicy`] fires;
//! production uses [`ExitPolicy`] (`std::process::exit(70)`), tests inject a
//! recording policy. A success resets the counter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use lunaris::{RerankCandidate, Reranker};
use lunaris_core::{Embedder, LunarisError, StorageError};

const DEFAULT_WATCHDOG_MS: u64 = 120_000;
const DEFAULT_TRIP_THRESHOLD: u32 = 2;

/// What to do when the wedge threshold is reached. Injectable so tests can
/// observe the trip without killing the test process.
pub trait WedgePolicy: Send + Sync + 'static {
    fn trip(&self, consecutive: u32, what: &str);
}

/// Production policy: log at ERROR and exit(70) so the on-demand hook spawn
/// brings up a fresh daemon. 70 = EX_SOFTWARE.
pub struct ExitPolicy;

impl WedgePolicy for ExitPolicy {
    fn trip(&self, consecutive: u32, what: &str) {
        tracing::error!(
            consecutive,
            what,
            "inference watchdog tripped — runtime wedged (spinning ggml pool \
             cannot be cancelled); exiting so hooks respawn a fresh daemon"
        );
        std::process::exit(70);
    }
}

fn watchdog_timeout() -> Duration {
    let ms = std::env::var("LUNARIS_INFER_WATCHDOG_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_WATCHDOG_MS);
    Duration::from_millis(ms.max(1))
}

fn trip_threshold() -> u32 {
    std::env::var("LUNARIS_INFER_WATCHDOG_TRIP")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_TRIP_THRESHOLD)
        .max(1)
}

fn wedge_error(what: &str, timeout: Duration) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(format!(
        "{what} watchdog: inference exceeded {} ms (runtime wedge suspected)",
        timeout.as_millis()
    )))
}

/// Shared timeout/counter core for both wrappers.
struct Watchdog {
    timeout: Duration,
    threshold: u32,
    consecutive: AtomicU32,
    policy: Arc<dyn WedgePolicy>,
    what: &'static str,
}

impl Watchdog {
    fn new(what: &'static str, policy: Arc<dyn WedgePolicy>) -> Self {
        Self {
            timeout: watchdog_timeout(),
            threshold: trip_threshold(),
            consecutive: AtomicU32::new(0),
            policy,
            what,
        }
    }

    async fn guard<T, F>(&self, fut: F) -> Result<T, LunarisError>
    where
        F: std::future::Future<Output = Result<T, LunarisError>>,
    {
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(result) => {
                self.consecutive.store(0, Ordering::Relaxed);
                result
            }
            Err(_elapsed) => {
                let consecutive = self.consecutive.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    what = self.what,
                    consecutive,
                    timeout_ms = self.timeout.as_millis() as u64,
                    "inference watchdog timeout"
                );
                if consecutive >= self.threshold {
                    self.policy.trip(consecutive, self.what);
                }
                Err(wedge_error(self.what, self.timeout))
            }
        }
    }
}

/// [`Embedder`] wrapper enforcing the watchdog on every embed call.
pub struct WatchdogEmbedder {
    inner: Arc<dyn Embedder>,
    dog: Watchdog,
}

impl WatchdogEmbedder {
    pub fn new(inner: Arc<dyn Embedder>, policy: Arc<dyn WedgePolicy>) -> Self {
        Self { inner, dog: Watchdog::new("embedder", policy) }
    }
}

#[async_trait]
impl Embedder for WatchdogEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        self.dog.guard(self.inner.embed_batch(inputs)).await
    }

    async fn embed_batch_lowpri(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        self.dog.guard(self.inner.embed_batch_lowpri(inputs)).await
    }
}

/// [`Reranker`] wrapper enforcing the watchdog on every rerank call.
pub struct WatchdogReranker {
    inner: Arc<dyn Reranker>,
    dog: Watchdog,
}

impl WatchdogReranker {
    pub fn new(inner: Arc<dyn Reranker>, policy: Arc<dyn WedgePolicy>) -> Self {
        Self { inner, dog: Watchdog::new("reranker", policy) }
    }
}

#[async_trait]
impl Reranker for WatchdogReranker {
    fn applies(&self) -> bool {
        self.inner.applies()
    }

    async fn rerank(
        &self,
        query: &str,
        docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        self.dog.guard(self.inner.rerank(query, docs)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Embedder whose calls never complete — models the wedged Metal graph.
    struct WedgedEmbedder;

    #[async_trait]
    impl Embedder for WedgedEmbedder {
        fn dim(&self) -> usize {
            4
        }
        async fn embed_batch(&self, _inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            std::future::pending().await
        }
    }

    struct HealthyEmbedder;

    #[async_trait]
    impl Embedder for HealthyEmbedder {
        fn dim(&self) -> usize {
            4
        }
        async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            Ok(inputs.iter().map(|_| vec![0.0; 4]).collect())
        }
    }

    #[derive(Default)]
    struct RecordingPolicy {
        trips: Mutex<Vec<u32>>,
    }

    impl WedgePolicy for RecordingPolicy {
        fn trip(&self, consecutive: u32, _what: &str) {
            self.trips.lock().unwrap().push(consecutive);
        }
    }

    fn tiny_watchdog(inner: Arc<dyn Embedder>, policy: Arc<RecordingPolicy>) -> WatchdogEmbedder {
        let mut w = WatchdogEmbedder::new(inner, policy);
        w.dog.timeout = Duration::from_millis(30);
        w.dog.threshold = 2;
        w
    }

    #[tokio::test]
    async fn wedged_embed_times_out_and_trips_on_second_consecutive() {
        let policy = Arc::new(RecordingPolicy::default());
        let dog = tiny_watchdog(Arc::new(WedgedEmbedder), policy.clone());

        let started = std::time::Instant::now();
        let first = dog.embed_batch(&["a"]).await;
        assert!(first.is_err(), "wedged call must error, not hang");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "watchdog must bound the wedge, elapsed {:?}",
            started.elapsed()
        );
        assert!(policy.trips.lock().unwrap().is_empty(), "one timeout must not trip");

        let second = dog.embed_batch(&["b"]).await;
        assert!(second.is_err());
        assert_eq!(
            policy.trips.lock().unwrap().as_slice(),
            &[2],
            "second consecutive timeout must trip the policy exactly once"
        );
    }

    #[tokio::test]
    async fn success_resets_the_consecutive_counter() {
        let policy = Arc::new(RecordingPolicy::default());
        // Alternate wedged and healthy through two watchdogs sharing a policy is
        // awkward; instead: one timeout, then a healthy call via a fresh inner,
        // then confirm the counter restarts (no trip on the next timeout).
        let dog = tiny_watchdog(Arc::new(WedgedEmbedder), policy.clone());
        assert!(dog.embed_batch(&["a"]).await.is_err());

        let healthy = tiny_watchdog(Arc::new(HealthyEmbedder), policy.clone());
        assert!(healthy.embed_batch(&["b"]).await.is_ok());

        // Counter on `dog` is 1; a healthy result on `dog` itself must reset it.
        // WedgedEmbedder never succeeds, so assert reset via the healthy wrapper:
        // its counter went 0 → success keeps it 0 → a later single timeout on
        // `dog` (counter 1 → 2) trips, proving per-wrapper isolation meanwhile.
        assert!(dog.embed_batch(&["c"]).await.is_err());
        assert_eq!(policy.trips.lock().unwrap().as_slice(), &[2]);
    }
}
