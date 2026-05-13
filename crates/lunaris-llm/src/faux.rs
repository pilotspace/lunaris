//! `FauxBackend` — shared test double for [`crate::LlmBackend`].
//!
//! Gated by `#[cfg(any(test, feature = "faux"))]` so it never appears in
//! production binaries. Enable the `faux` Cargo feature in
//! `[dev-dependencies]` for consuming crates:
//!
//! ```toml
//! # Cargo.toml of the consuming crate
//! [dev-dependencies]
//! lunaris-llm = { workspace = true, features = ["faux"] }
//! ```
//!
//! # Example
//!
//! ```rust
//! # use std::sync::Arc;
//! # use lunaris_llm::{LlmBackend, FauxBackend};
//! # #[tokio::main]
//! # async fn main() {
//! // Build a backend that returns a fixed JSON string.
//! let backend: Arc<dyn LlmBackend> = Arc::new(
//!     FauxBackend::new()
//!         .with_response(r#"{"entities":[],"relations":[]}"#)
//!         .with_model_id("faux://extractor"),
//! );
//!
//! // After calling it, assert what the backend saw.
//! let result = backend
//!     .generate("some prompt", lunaris_llm::SchemaConstraint::None, Default::default())
//!     .await
//!     .unwrap();
//! # drop(result);
//! # }
//! ```
//!
//! See [`FauxBackend`] for the full builder API.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::LunarisError;
use parking_lot::Mutex;

use crate::{GenOpts, LlmBackend, SchemaConstraint};

// ── Captured call state ────────────────────────────────────────────────────

/// Snapshot of the most recent [`LlmBackend::generate`] call.
///
/// Inspect via [`FauxBackend::last_prompt`], [`FauxBackend::last_constraint_tag`],
/// [`FauxBackend::last_opts`], and [`FauxBackend::call_count`].
#[derive(Debug, Default, Clone)]
struct Captured {
    last_prompt: Option<String>,
    /// Discriminator tag: `"none"`, `"gbnf"`, or `"json_schema"`.
    last_constraint_tag: Option<&'static str>,
    last_opts: Option<GenOpts>,
    call_count: usize,
}

// ── FauxBackend ────────────────────────────────────────────────────────────

/// A configurable, capturing test double for [`LlmBackend`].
///
/// ## Response queue
///
/// Responses are popped from a FIFO queue with
/// [`with_response`](FauxBackend::with_response) /
/// [`with_error`](FauxBackend::with_error). When the queue is drained, the
/// backend returns `Ok(String::new())` (an empty success). Wire multiple
/// calls in order:
///
/// ```rust
/// # use lunaris_llm::FauxBackend;
/// let fb = FauxBackend::new()
///     .with_response("first")
///     .with_response("second")
///     .with_error_msg("network timeout");
/// ```
///
/// ## Delay (timeout tests)
///
/// [`with_delay_ms`](FauxBackend::with_delay_ms) injects a `tokio::time::sleep`
/// before *every* response so callers can test timeout / cancellation paths.
///
/// ## Capture assertions
///
/// After one or more calls, read what the backend received:
///
/// ```rust
/// # use std::sync::Arc;
/// # use lunaris_llm::{LlmBackend, FauxBackend, SchemaConstraint};
/// # #[tokio::main]
/// # async fn main() {
/// let fb = Arc::new(FauxBackend::new().with_response("ok"));
/// let _ = fb.generate("hello", SchemaConstraint::None, Default::default()).await;
/// assert_eq!(fb.last_prompt().as_deref(), Some("hello"));
/// assert_eq!(fb.last_constraint_tag(), Some("none"));
/// assert_eq!(fb.call_count(), 1);
/// # }
/// ```
pub struct FauxBackend {
    model_id: String,
    applies: bool,
    delay_ms: u64,
    /// Queued responses (popped FIFO). Each entry is either `Ok(String)` or
    /// `Err(LunarisError)`.
    queue: Mutex<VecDeque<Result<String, LunarisError>>>,
    captured: Mutex<Captured>,
}

impl std::fmt::Debug for FauxBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let q = self.queue.lock();
        let c = self.captured.lock();
        f.debug_struct("FauxBackend")
            .field("model_id", &self.model_id)
            .field("applies", &self.applies)
            .field("delay_ms", &self.delay_ms)
            .field("queue_len", &q.len())
            .field("call_count", &c.call_count)
            .finish()
    }
}

impl Default for FauxBackend {
    fn default() -> Self {
        Self {
            model_id: "faux://test".to_owned(),
            applies: true,
            delay_ms: 0,
            queue: Mutex::new(VecDeque::new()),
            captured: Mutex::new(Captured::default()),
        }
    }
}

impl FauxBackend {
    /// Create a new [`FauxBackend`] with default settings:
    /// - `model_id = "faux://test"`
    /// - `applies = true`
    /// - no delay
    /// - empty queue (returns `""` when drained)
    pub fn new() -> Self {
        Self::default()
    }

    // ── Builder knobs ──────────────────────────────────────────────────

    /// Override the string returned by [`LlmBackend::model_id`].
    pub fn with_model_id(mut self, id: impl Into<String>) -> Self {
        self.model_id = id.into();
        self
    }

    /// Override the bool returned by [`LlmBackend::applies`].
    pub fn with_applies(mut self, v: bool) -> Self {
        self.applies = v;
        self
    }

    /// Enqueue a successful `Ok(text)` response.
    ///
    /// Multiple calls are consumed FIFO. When the queue is empty the backend
    /// returns `Ok(String::new())`.
    pub fn with_response(self, text: impl Into<String>) -> Self {
        self.queue.lock().push_back(Ok(text.into()));
        self
    }

    /// Enqueue an `Err(LunarisError::Storage(StorageError::Backend(msg)))`.
    ///
    /// Convenient for transport-error tests that just need *an* error with a
    /// recognisable substring; see [`with_error`] for a custom error variant.
    pub fn with_error_msg(self, msg: impl Into<String>) -> Self {
        use lunaris_core::StorageError;
        self.queue.lock().push_back(Err(LunarisError::Storage(StorageError::Backend(msg.into()))));
        self
    }

    /// Enqueue a fully custom [`LunarisError`] response.
    pub fn with_error(self, err: LunarisError) -> Self {
        self.queue.lock().push_back(Err(err));
        self
    }

    /// Sleep this many milliseconds before returning *every* response.
    /// Pass a value longer than the caller's timeout to trigger timeout paths.
    pub fn with_delay_ms(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }

    // ── Capture accessors ──────────────────────────────────────────────

    /// The prompt string from the most recent `generate` call, or `None` if
    /// `generate` has never been called.
    pub fn last_prompt(&self) -> Option<String> {
        self.captured.lock().last_prompt.clone()
    }

    /// The [`SchemaConstraint`] discriminator tag from the most recent call.
    ///
    /// One of `"none"`, `"gbnf"`, `"json_schema"`, or `None` if not yet called.
    pub fn last_constraint_tag(&self) -> Option<&'static str> {
        self.captured.lock().last_constraint_tag
    }

    /// The [`GenOpts`] from the most recent call, or `None` if not yet called.
    pub fn last_opts(&self) -> Option<GenOpts> {
        self.captured.lock().last_opts
    }

    /// How many times `generate` has been called since construction.
    pub fn call_count(&self) -> usize {
        self.captured.lock().call_count
    }
}

#[async_trait]
impl LlmBackend for FauxBackend {
    async fn generate(
        &self,
        prompt: &str,
        constraint: SchemaConstraint<'_>,
        opts: GenOpts,
    ) -> Result<String, LunarisError> {
        // Apply optional delay BEFORE taking any lock — satisfies the
        // CLAUDE.md "never hold a lock across .await" rule.
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }

        // Capture the call state.  Lock is acquired, updated, and dropped
        // before any async boundary.
        let constraint_tag = match constraint {
            SchemaConstraint::None => "none",
            SchemaConstraint::Gbnf(_) => "gbnf",
            SchemaConstraint::JsonSchema(_) => "json_schema",
        };
        {
            let mut cap = self.captured.lock();
            cap.last_prompt = Some(prompt.to_owned());
            cap.last_constraint_tag = Some(constraint_tag);
            cap.last_opts = Some(opts);
            cap.call_count += 1;
        }

        // Pop from queue; default to Ok("") when drained.
        let response = self.queue.lock().pop_front().unwrap_or_else(|| Ok(String::new()));
        response
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn applies(&self) -> bool {
        self.applies
    }
}

// ── Unit tests for FauxBackend itself ──────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn returns_queued_responses_fifo() {
        let fb = FauxBackend::new().with_response("first").with_response("second");
        let r1 = fb.generate("p", SchemaConstraint::None, GenOpts::default()).await.unwrap();
        let r2 = fb.generate("p", SchemaConstraint::None, GenOpts::default()).await.unwrap();
        assert_eq!(r1, "first");
        assert_eq!(r2, "second");
    }

    #[tokio::test]
    async fn returns_empty_string_when_queue_drained() {
        let fb = FauxBackend::new();
        let r = fb.generate("p", SchemaConstraint::None, GenOpts::default()).await.unwrap();
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn queued_error_propagates() {
        let fb = FauxBackend::new().with_error_msg("simulated timeout");
        let err = fb
            .generate("p", SchemaConstraint::None, GenOpts::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated timeout"));
    }

    #[tokio::test]
    async fn captures_prompt_and_constraint_tag() {
        let fb = FauxBackend::new().with_response("ok");
        let _ = fb.generate("hello world", SchemaConstraint::Gbnf("root ::= \"{}\""), GenOpts::default()).await;
        assert_eq!(fb.last_prompt().as_deref(), Some("hello world"));
        assert_eq!(fb.last_constraint_tag(), Some("gbnf"));
        assert_eq!(fb.call_count(), 1);
    }

    #[tokio::test]
    async fn call_count_increments() {
        let fb = FauxBackend::new();
        for _ in 0..5 {
            let _ = fb.generate("p", SchemaConstraint::None, GenOpts::default()).await;
        }
        assert_eq!(fb.call_count(), 5);
    }

    #[tokio::test]
    async fn applies_and_model_id_configurable() {
        let fb = FauxBackend::new()
            .with_applies(false)
            .with_model_id("faux://custom");
        assert!(!fb.applies());
        assert_eq!(fb.model_id(), "faux://custom");
    }

    #[test]
    fn faux_backend_is_send_sync() {
        fn _assert<T: Send + Sync + 'static>() {}
        _assert::<FauxBackend>();
    }

    #[test]
    fn arc_dyn_llm_backend_compiles() {
        let _: Arc<dyn LlmBackend> = Arc::new(FauxBackend::new().with_response("ok"));
    }
}
