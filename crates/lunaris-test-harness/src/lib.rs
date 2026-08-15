//! Moon-backed test fixtures — the substrate every test in the workspace runs on.
//!
//! Through 0.6.x this crate had two jobs: hand out a real, disposable
//! single-shard Moon, and **degrade to `memory://`** on machines with no `moon`
//! binary. 0.7.0 deleted the embedded SQLite backend, so the second job is
//! gone. What replaced it is not a silent skip — it is a panic that tells you
//! how to get a binary.
//!
//! ## The idiom
//!
//! ```ignore
//! use lunaris_test_harness::open_test_engine;
//!
//! #[tokio::test]
//! async fn my_test() {
//!     let engine = open_test_engine().await;      // one private Moon, ~3 ms
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
//! There is one substrate, so there is one outcome: a fixture either gets a
//! Moon or the test panics. [`BACKEND_ENV`] survives only to keep a stale
//! `LUNARIS_TEST_BACKEND=memory` in someone's shell profile from being
//! *ignored*:
//!
//! | `LUNARIS_TEST_BACKEND` | behaviour                                        |
//! |------------------------|--------------------------------------------------|
//! | unset / `auto` / `moon`| a disposable child-process Moon, or panic         |
//! | `memory`               | **panic**, naming the 0.7.0 removal               |
//! | anything else          | **panic** — a typo is never a silent `auto`       |
//!
//! Accepting `memory` as a no-op would be the worst outcome available: the
//! caller asked for a substrate that does not exist and would silently get the
//! other one.
//!
//! The binary is `$MOON_TEST_BINARY`, else `vendor/moon/target/{release,debug}/moon`.
//! Nothing here compiles the Moon **server** into a test binary — see
//! [`moon`] for why that rules out the `embedded-moon` feature.

#![forbid(unsafe_code)]

pub mod doubles;
pub mod moon;

use std::ops::Deref;
use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::{Embedder, StoragePort, StubEmbedder};

pub use moon::{EphemeralMoon, MOON_BINARY_ENV, RESERVED_PORTS, moon_binary};

/// Env var that used to select a backend. See the crate-level table: the only
/// values it still accepts are the ones that mean "Moon".
pub const BACKEND_ENV: &str = "LUNARIS_TEST_BACKEND";

/// Vector dimension of the default fixture embedder.
///
/// Matches granite-r2, the production embedder, so a Moon FT index created by a
/// fixture has the same shape as a production one.
pub const DEFAULT_TEST_DIM: usize = 768;

/// Validate `LUNARIS_TEST_BACKEND`'s value.
///
/// Pure on purpose: the process environment is read by the caller, so unit
/// tests can exercise every branch without mutating it (mutating env is an
/// `unsafe fn` in edition 2024, forbidden in this crate).
///
/// # Errors
/// `memory` — the substrate it named was deleted in 0.7.0 — and any value that
/// is not (case-insensitively) `auto`, `moon`, or blank.
pub fn check_backend_env_value(raw: Option<&str>) -> Result<(), String> {
    match raw.map(str::trim).unwrap_or("") {
        "" => Ok(()),
        v if v.eq_ignore_ascii_case("auto") || v.eq_ignore_ascii_case("moon") => Ok(()),
        v if v.eq_ignore_ascii_case("memory") => Err(format!(
            "{BACKEND_ENV}=memory was removed in 0.7.0 along with the embedded SQLite \
             backend (`lunaris-storage-embedded`) it selected. There is no in-process \
             substrate left to fall back to: every fixture now runs against a disposable \
             child-process Moon. Unset {BACKEND_ENV} (or set it to `moon`) and make a moon \
             binary reachable — see ${MOON_BINARY_ENV}."
        )),
        other => Err(format!("{BACKEND_ENV}={other:?} is not one of: auto, moon")),
    }
}

/// Validate the process environment. Called by every fixture constructor.
///
/// # Panics
/// On a rejected `LUNARIS_TEST_BACKEND` — see [`check_backend_env_value`].
fn check_backend_env() {
    let raw = std::env::var(BACKEND_ENV).ok();
    if let Err(e) = check_backend_env_value(raw.as_deref()) {
        panic!("{e}");
    }
}

/// The message a fixture dies with when no Moon could be started.
///
/// Split out so the wording is asserted by a test rather than trusted. It must
/// name the env var AND the build command: "no moon binary" alone sends the
/// reader to grep the harness.
fn no_moon_panic(cause: &anyhow::Error) -> String {
    format!(
        "lunaris-test-harness: could not start an ephemeral Moon, and 0.7.0 removed the \
         `memory://` fallback that used to absorb this.\n  cause: {cause:#}\n  \
         fix:   build the pinned submodule once —\n           \
         git submodule update --init vendor/moon\n           \
         cargo build --release --bin moon --manifest-path vendor/moon/Cargo.toml\n         \
         then let the harness discover vendor/moon/target/release/moon, or point it at a \
         binary explicitly:\n           export {MOON_BINARY_ENV}=/path/to/moon\n  \
         note:  inside a LINKED git worktree the submodule build fails (the outer \
         workspace claims it) — build from the primary checkout and set \
         {MOON_BINARY_ENV}. See docs/testing/memory-to-moon-port-plan.md §1."
    )
}

/// A storage URL plus the process that keeps it alive.
///
/// Drop order matters: the Moon child dies with this value, so keep it in scope
/// for as long as anything holds a connection to [`Self::url`].
#[derive(Debug)]
pub struct TestStore {
    url: String,
    moon: EphemeralMoon,
}

impl TestStore {
    /// The URL to open — `moon://127.0.0.1:<port>`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The underlying Moon fixture.
    #[must_use]
    pub fn moon(&self) -> &EphemeralMoon {
        &self.moon
    }
}

/// Spawn a disposable Moon and return its URL.
///
/// # Panics
/// If `LUNARIS_TEST_BACKEND` names a removed backend, or no Moon could be
/// started. The panic message names `MOON_TEST_BINARY` and the one-line
/// `cargo build --release --bin moon` that fixes it — there is no
/// `memory://` fallback to absorb a missing binary since 0.7.0.
pub async fn open_test_store() -> TestStore {
    check_backend_env();
    let moon = EphemeralMoon::spawn().await.unwrap_or_else(|e| panic!("{}", no_moon_panic(&e)));
    TestStore { url: moon.url().to_owned(), moon }
}

/// A bare `StoragePort` bound to a [`TestStore`], which it keeps alive.
///
/// For the test files that reach past the engine and open a backend directly.
pub struct TestStorage {
    port: Arc<dyn StoragePort>,
    store: TestStore,
}

/// Manual — `StoragePort` has no `Debug` supertrait, so the derive cannot see
/// through the `Arc<dyn _>`. The store (the URL) is the useful half.
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
    open_test_storage_with_dim(DEFAULT_TEST_DIM).await
}

/// Open a bare backend with an explicit vector dimension.
///
/// `dim` is load-bearing: Moon fixes its FT index width at `FT.CREATE` time and
/// will NOT resize an existing index, so a fixture whose embedder is not 768-d
/// must say so here.
///
/// # Panics
/// If the store cannot be opened or the backend refuses the connection.
pub async fn open_test_storage_with_dim(dim: usize) -> TestStorage {
    let store = open_test_store().await;
    let port: Arc<dyn StoragePort> = Arc::new(
        lunaris_storage_moon::MoonStorage::connect_with_dim(store.url(), dim)
            .await
            .unwrap_or_else(|e| panic!("connect Moon at {}: {e}", store.url())),
    );
    TestStorage { port, store }
}

/// A `Lunaris` handle bound to a [`TestStore`], which it keeps alive.
///
/// Derefs to [`Lunaris`], so it is a drop-in for a handle produced by
/// `Lunaris::open(url)`.
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
/// If the store cannot be opened — see [`open_test_store`].
pub async fn open_test_engine() -> TestEngine {
    open_test_engine_with_embedder(Arc::new(StubEmbedder::new(DEFAULT_TEST_DIM))).await
}

/// Like [`open_test_engine`] but with a caller-supplied embedder.
///
/// # Panics
/// If the store cannot be opened, or `Lunaris` fails to open against it.
pub async fn open_test_engine_with_embedder(embedder: Arc<dyn Embedder>) -> TestEngine {
    let store = open_test_store().await;
    let engine = Lunaris::open_with_embedder(store.url(), embedder)
        .await
        .unwrap_or_else(|e| panic!("open engine on {}: {e}", store.url()));
    TestEngine { engine, store }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_blank_auto_and_moon_are_accepted() {
        assert_eq!(check_backend_env_value(None), Ok(()));
        assert_eq!(check_backend_env_value(Some("")), Ok(()));
        assert_eq!(check_backend_env_value(Some("  ")), Ok(()));
        assert_eq!(check_backend_env_value(Some("auto")), Ok(()));
        assert_eq!(check_backend_env_value(Some("MOON")), Ok(()));
        assert_eq!(check_backend_env_value(Some(" moon ")), Ok(()));
    }

    /// The whole point of keeping [`BACKEND_ENV`] alive: a shell profile that
    /// still exports `memory` must FAIL, not be quietly upgraded to Moon.
    #[test]
    fn memory_is_a_named_hard_error_not_a_silent_upgrade() {
        let err = check_backend_env_value(Some("memory")).expect_err("memory must be rejected");
        assert!(err.contains("removed in 0.7.0"), "{err}");
        assert!(err.contains("lunaris-storage-embedded"), "{err}");
        assert!(err.contains(MOON_BINARY_ENV), "{err}");
        // Case-insensitively, too — the removal is not spelling-sensitive.
        assert!(check_backend_env_value(Some("Memory")).is_err());
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_auto() {
        let err = check_backend_env_value(Some("mooon")).expect_err("typo must not parse");
        assert!(err.contains("mooon"), "error must echo the bad value: {err}");
    }

    /// A missing binary is the one failure a developer hits cold. The message
    /// has to carry the fix, not just the symptom.
    #[test]
    fn no_moon_message_carries_the_fix() {
        let msg = no_moon_panic(&anyhow::anyhow!("no moon binary"));
        assert!(msg.contains(MOON_BINARY_ENV), "{msg}");
        assert!(msg.contains("vendor/moon/target/release/moon"), "{msg}");
        assert!(msg.contains("cargo build --release --bin moon"), "{msg}");
    }
}
