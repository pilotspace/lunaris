//! Moon-backed test fixtures — the drop-in replacement for `memory://`.
//!
//! 0.7.0 deletes the embedded SQLite backend (`memory://`, `sqlite:///path`).
//! Roughly sixty test files across the workspace open storage through it, so
//! the deletion is gated on those files having somewhere else to go. This crate
//! is that somewhere: it hands out a **real, disposable, single-shard Moon**,
//! one per fixture, and transparently degrades to `memory://` on machines that
//! have no `moon` binary.
//!
//! ## The idiom
//!
//! ```ignore
//! use lunaris_test_harness::open_test_engine;
//!
//! #[tokio::test]
//! async fn my_test() {
//!     let engine = open_test_engine().await;      // was: Lunaris::open("memory://")
//!     let scoped = engine.scoped(Scope::new("my-test").unwrap());
//!     // ... `engine` derefs to `Lunaris`; hold it for the test's lifetime.
//! }
//! ```
//!
//! Holding the returned value matters: it owns the Moon child process, and
//! dropping it reaps the process and deletes the data directory.
//!
//! ## Backend selection
//!
//! | `LUNARIS_TEST_BACKEND` | behaviour                                            |
//! |------------------------|------------------------------------------------------|
//! | unset / `auto`         | Moon when a binary resolves, else `memory://`         |
//! | `moon`                 | Moon or **panic** — the CI gate; never silently skips |
//! | `memory`               | always `memory://`                                    |
//!
//! The binary is `$MOON_TEST_BINARY`, else `vendor/moon/target/{release,debug}/moon`.
//! Nothing here compiles the Moon **server** into a test binary — see
//! [`moon`] for why that rules out the `embedded-moon` feature.
//!
//! ## Why a policy argument and not just env
//!
//! Tests in one binary share a process, and mutating the environment is an
//! `unsafe fn` in edition 2024 (forbidden here). So env is read exactly once
//! per call, at the top, and every knob is also reachable through an explicit
//! [`Policy`] argument — the same DI shape `decide_storage_with_launcher` uses.

#![forbid(unsafe_code)]

pub mod moon;

use std::ops::Deref;
use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::{Embedder, StoragePort, StubEmbedder};

pub use moon::{EphemeralMoon, MOON_BINARY_ENV, RESERVED_PORTS, moon_binary};

/// Env var selecting the backend policy. See the crate-level table.
pub const BACKEND_ENV: &str = "LUNARIS_TEST_BACKEND";

/// Vector dimension of the default fixture embedder.
///
/// Matches granite-r2, the production embedder, so a Moon FT index created by a
/// fixture has the same shape as a production one.
pub const DEFAULT_TEST_DIM: usize = 768;

/// Which substrate a fixture actually resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// A disposable child-process Moon.
    Moon,
    /// The embedded SQLite backend (`memory://`) — the fallback path.
    Memory,
}

/// How hard to insist on Moon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    /// Moon when available, `memory://` otherwise. The default, and what keeps
    /// `cargo test --workspace` green on a machine with no Moon binary.
    #[default]
    Auto,
    /// Moon or panic. Set `LUNARIS_TEST_BACKEND=moon` in CI so a lost binary
    /// surfaces as a failure instead of a silent downgrade.
    RequireMoon,
    /// Never spawn Moon.
    ForceMemory,
}

impl Policy {
    /// Parse a `LUNARIS_TEST_BACKEND` value.
    ///
    /// Pure on purpose: the process environment is read by the caller, so unit
    /// tests can exercise every branch without mutating it.
    ///
    /// # Errors
    /// Any value other than (case-insensitive) `auto`, `moon`, `memory`, or
    /// blank. Silently treating a typo as `auto` would turn "CI runs against
    /// Moon" into a claim nobody checks.
    pub fn from_env_value(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).unwrap_or("") {
            "" | "auto" => Ok(Self::Auto),
            other if other.eq_ignore_ascii_case("auto") => Ok(Self::Auto),
            other if other.eq_ignore_ascii_case("moon") => Ok(Self::RequireMoon),
            other if other.eq_ignore_ascii_case("memory") => Ok(Self::ForceMemory),
            other => Err(format!("{BACKEND_ENV}={other:?} is not one of: auto, moon, memory")),
        }
    }

    /// Read the policy from the process environment.
    ///
    /// # Panics
    /// On an unrecognised value — see [`Self::from_env_value`].
    #[must_use]
    pub fn from_env() -> Self {
        let raw = std::env::var(BACKEND_ENV).ok();
        Self::from_env_value(raw.as_deref()).unwrap_or_else(|e| panic!("{e}"))
    }
}

/// A storage URL plus whatever process keeps it alive.
///
/// Drop order matters: the Moon child dies with this value, so keep it in scope
/// for as long as anything holds a connection to [`Self::url`].
#[derive(Debug)]
pub struct TestStore {
    url: String,
    backend: Backend,
    moon: Option<EphemeralMoon>,
}

impl TestStore {
    /// The URL to open — `moon://127.0.0.1:<port>` or `memory://`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Which substrate this resolved to.
    #[must_use]
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// The underlying Moon fixture, when this is a Moon-backed store.
    #[must_use]
    pub fn moon(&self) -> Option<&EphemeralMoon> {
        self.moon.as_ref()
    }
}

/// Resolve a store using the environment's [`Policy`].
///
/// # Panics
/// Under `LUNARIS_TEST_BACKEND=moon` with no usable binary, or if Moon fails
/// to boot.
pub async fn open_test_store() -> TestStore {
    open_test_store_with(Policy::from_env()).await
}

/// Resolve a store under an explicit policy.
///
/// # Panics
/// Under [`Policy::RequireMoon`] when no binary resolves or Moon fails to boot.
/// Under [`Policy::Auto`] a boot failure degrades to `memory://` with a message
/// on stderr — a broken submodule build must not turn the whole suite red.
pub async fn open_test_store_with(policy: Policy) -> TestStore {
    let memory = || TestStore { url: "memory://".to_owned(), backend: Backend::Memory, moon: None };
    match policy {
        Policy::ForceMemory => memory(),
        Policy::Auto if moon_binary().is_none() => memory(),
        Policy::Auto => match EphemeralMoon::spawn().await {
            Ok(m) => TestStore { url: m.url().to_owned(), backend: Backend::Moon, moon: Some(m) },
            Err(e) => {
                eprintln!("lunaris-test-harness: falling back to memory:// ({e:#})");
                memory()
            }
        },
        Policy::RequireMoon => {
            let m = EphemeralMoon::spawn()
                .await
                .unwrap_or_else(|e| panic!("{BACKEND_ENV}=moon but Moon did not start: {e:#}"));
            TestStore { url: m.url().to_owned(), backend: Backend::Moon, moon: Some(m) }
        }
    }
}

/// A bare `StoragePort` bound to a [`TestStore`], which it keeps alive.
///
/// For the test files that reach past the engine and call
/// `EmbeddedStorage::connect("memory://")` directly.
pub struct TestStorage {
    port: Arc<dyn StoragePort>,
    store: TestStore,
}

/// Manual — `StoragePort` has no `Debug` supertrait, so the derive cannot see
/// through the `Arc<dyn _>`. The store (URL + backend) is the useful half.
impl std::fmt::Debug for TestStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestStorage").field("store", &self.store).finish_non_exhaustive()
    }
}

impl TestStorage {
    /// A clonable handle to the backend.
    #[must_use]
    pub fn port(&self) -> Arc<dyn StoragePort> {
        Arc::clone(&self.port)
    }

    /// The URL this backend was opened against.
    #[must_use]
    pub fn url(&self) -> &str {
        self.store.url()
    }

    /// Which substrate this resolved to.
    #[must_use]
    pub fn backend(&self) -> Backend {
        self.store.backend()
    }
}

impl Deref for TestStorage {
    type Target = Arc<dyn StoragePort>;

    fn deref(&self) -> &Self::Target {
        &self.port
    }
}

/// Open a bare backend on a fresh store, sized to [`DEFAULT_TEST_DIM`].
///
/// # Panics
/// If the store cannot be opened or the backend refuses the connection.
pub async fn open_test_storage() -> TestStorage {
    open_test_storage_with(Policy::from_env(), DEFAULT_TEST_DIM).await
}

/// Open a bare backend with an explicit policy and vector dimension.
///
/// `dim` is load-bearing on Moon and inert on the embedded backend: Moon fixes
/// its FT index width at `FT.CREATE` time and will NOT resize an existing
/// index, so a fixture whose embedder is not 768-d must say so here.
///
/// # Panics
/// If the store cannot be opened or the backend refuses the connection.
pub async fn open_test_storage_with(policy: Policy, dim: usize) -> TestStorage {
    let store = open_test_store_with(policy).await;
    let port: Arc<dyn StoragePort> = match store.backend() {
        Backend::Moon => Arc::new(
            lunaris_storage_moon::MoonStorage::connect_with_dim(store.url(), dim)
                .await
                .unwrap_or_else(|e| panic!("connect Moon at {}: {e}", store.url())),
        ),
        Backend::Memory => Arc::new(
            lunaris_storage_embedded::EmbeddedStorage::connect(store.url())
                .await
                .unwrap_or_else(|e| panic!("connect embedded at {}: {e}", store.url())),
        ),
    };
    TestStorage { port, store }
}

/// A `Lunaris` handle bound to a [`TestStore`], which it keeps alive.
///
/// Derefs to [`Lunaris`], so it is a literal drop-in for a handle produced by
/// `Lunaris::open("memory://")`.
#[derive(Debug)]
pub struct TestEngine {
    engine: Lunaris,
    store: TestStore,
}

impl TestEngine {
    /// The URL this engine was opened against.
    #[must_use]
    pub fn url(&self) -> &str {
        self.store.url()
    }

    /// Which substrate this engine is talking to.
    #[must_use]
    pub fn backend(&self) -> Backend {
        self.store.backend()
    }

    /// Split into the engine and its store guard.
    ///
    /// For callers that need `Arc<Lunaris>` (e.g. `WorkingMemory::new`). The
    /// returned [`TestStore`] must outlive the `Arc`, or the Moon behind it
    /// dies mid-test.
    #[must_use]
    pub fn into_parts(self) -> (Lunaris, TestStore) {
        (self.engine, self.store)
    }
}

impl Deref for TestEngine {
    type Target = Lunaris;

    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

/// Open an engine on a fresh store with a deterministic 768-d [`StubEmbedder`].
///
/// The stub (not the real granite-r2 GGUF) is deliberate: fixtures must not
/// depend on a staged model, and Moon sizes its FT indices from
/// `embedder.dim()` at first open.
///
/// # Panics
/// If the store cannot be opened — see [`open_test_store_with`].
pub async fn open_test_engine() -> TestEngine {
    open_test_engine_with_embedder(Arc::new(StubEmbedder::new(DEFAULT_TEST_DIM))).await
}

/// Like [`open_test_engine`] but with a caller-supplied embedder.
///
/// # Panics
/// If the store cannot be opened, or `Lunaris` fails to open against it.
pub async fn open_test_engine_with_embedder(embedder: Arc<dyn Embedder>) -> TestEngine {
    open_test_engine_with(Policy::from_env(), embedder).await
}

/// Fully explicit constructor — policy and embedder both supplied.
///
/// # Panics
/// If the store cannot be opened, or `Lunaris` fails to open against it.
pub async fn open_test_engine_with(policy: Policy, embedder: Arc<dyn Embedder>) -> TestEngine {
    let store = open_test_store_with(policy).await;
    let engine = Lunaris::open_with_embedder(store.url(), embedder)
        .await
        .unwrap_or_else(|e| panic!("open engine on {}: {e}", store.url()));
    TestEngine { engine, store }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_and_blank_resolve_to_auto() {
        assert_eq!(Policy::from_env_value(None), Ok(Policy::Auto));
        assert_eq!(Policy::from_env_value(Some("")), Ok(Policy::Auto));
        assert_eq!(Policy::from_env_value(Some("  ")), Ok(Policy::Auto));
    }

    #[test]
    fn known_values_parse_case_insensitively() {
        assert_eq!(Policy::from_env_value(Some("moon")), Ok(Policy::RequireMoon));
        assert_eq!(Policy::from_env_value(Some("MOON")), Ok(Policy::RequireMoon));
        assert_eq!(Policy::from_env_value(Some("Memory")), Ok(Policy::ForceMemory));
        assert_eq!(Policy::from_env_value(Some(" auto ")), Ok(Policy::Auto));
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_auto() {
        let err = Policy::from_env_value(Some("mooon")).expect_err("typo must not parse");
        assert!(err.contains("mooon"), "error must echo the bad value: {err}");
    }
}
