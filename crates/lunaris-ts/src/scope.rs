//! Wave 3G — handwritten napi-rs 3.x bindings for the v0.2 multi-agent
//! partitioning surface: `Scope`, `EpisodeBuilder`, `ScopedLunaris`.
//!
//! These are NOT codegen-managed for the same reasons as the PyO3 counterpart
//! (`crates/lunaris-py/src/scope.rs`):
//!
//! - `Scope` is a validating newtype — constructor returns `Result`
//! - `EpisodeBuilder` has a `pub(crate) into_episode` terminal
//! - `ScopedLunaris<'a>` borrows — napi-rs `#[napi]` structs need `'static`
//!
//! ## Lifetime strategy
//!
//! `NapiScopedLunaris` owns `Arc<lunaris::Lunaris> + lunaris::Scope`. On each
//! async method call it calls `inner.scoped(scope.clone())` to derive the
//! borrowed `ScopedLunaris<'_>` locally inside the async block. No lifetimes
//! escape the Future boundary.
//!
//! ## Async discipline (CLAUDE.md)
//!
//! napi-rs 3.x routes `pub async fn` through the shared tokio runtime via the
//! `tokio_rt` feature. Every `.await` is awaited inside the method body.

use std::sync::Arc;

use napi_derive::napi;

use crate::errors::napi_err;
use crate::generated::Lunaris;

// ---------------------------------------------------------------------------
// NapiScope
// ---------------------------------------------------------------------------

/// Multi-agent / multi-tenant partition key (RFC 0001).
///
/// Validates against `^[A-Za-z0-9_\\-:.]{1,128}$`. Throws on construction if
/// the string is empty, > 128 chars, or contains a character outside the
/// allowed set.
///
/// ```ts
/// import { Scope } from 'lunaris';
///
/// const s = Scope.new("acme:agent-42");   // OK
/// Scope.new("");                           // throws
/// Scope.new("a".repeat(129));             // throws
/// ```
#[napi]
pub struct Scope {
    pub(crate) inner: ::lunaris::Scope,
}

#[napi]
impl Scope {
    /// Construct a `Scope` from `s`.
    ///
    /// Validates `^[A-Za-z0-9_\\-:.]{1,128}$`. Throws `Error` if the string
    /// is empty, longer than 128 bytes, or contains an invalid character.
    #[napi(factory)]
    pub fn new(s: String) -> napi::Result<Self> {
        let inner = ::lunaris::Scope::new(&s).map_err(|e| {
            napi::Error::from_reason(format!("Scope: {e}"))
        })?;
        Ok(Self { inner })
    }

    /// Return the scope as a plain string.
    #[napi]
    pub fn as_str(&self) -> &str {
        self.inner.as_str()
    }

    /// Return the scope as a plain string (alias for ergonomic `.toString()` TS usage).
    #[napi(js_name = "toString")]
    pub fn to_string_js(&self) -> String {
        self.inner.as_str().to_string()
    }
}

// ---------------------------------------------------------------------------
// NapiEpisodeBuilder
// ---------------------------------------------------------------------------

/// Scope-less payload builder for an `Episode`.
///
/// Assemble all Episode fields **except** scope using this builder. The scope
/// is injected exactly once by `ScopedLunaris.ingest`. This makes it
/// impossible to construct an Episode with an arbitrary scope by bypassing
/// the `ScopedLunaris` wrapper.
///
/// ```ts
/// import { EpisodeBuilder, Scope } from 'lunaris';
///
/// const builder = new EpisodeBuilder("agent:fs/report.md", "# Q3 Report")
///   .tRef("2026-01-01T00:00:00Z")
///   .metadata({ author: "helios" });
///
/// const lsn = await engine.scoped(Scope.new("acme:agent-1")).ingest(builder);
/// ```
#[napi]
pub struct EpisodeBuilder {
    pub(crate) inner: ::lunaris::EpisodeBuilder,
}

#[napi]
impl EpisodeBuilder {
    /// Construct a builder with the required `source` and `content`.
    ///
    /// `source` is the namespace-qualified origin (e.g. `"helios:fs/report.md"`).
    /// `content` is the raw text that will be chunked + embedded.
    #[napi(constructor)]
    pub fn new(source: String, content: String) -> Self {
        Self { inner: ::lunaris::EpisodeBuilder::new(source, content) }
    }

    /// Set the reference timestamp (valid time anchor).
    ///
    /// Accepts an ISO-8601 string (e.g. `"2026-01-01T00:00:00Z"`). When not
    /// set the ingest pipeline uses the current wall time from the HlcClock.
    ///
    /// Returns a new `EpisodeBuilder` (builder pattern).
    #[napi(js_name = "tRef")]
    pub fn t_ref(&self, t: String) -> napi::Result<EpisodeBuilder> {
        let dt: chrono::DateTime<chrono::Utc> = t.parse().map_err(|e| {
            napi::Error::from_reason(format!(
                "EpisodeBuilder.tRef: invalid ISO-8601 datetime {t:?}: {e}"
            ))
        })?;
        Ok(EpisodeBuilder { inner: self.inner.clone().t_ref(dt) })
    }

    /// Merge metadata key/value pairs into the builder.
    ///
    /// `m` must be a JSON object. Values must be JSON-serialisable.
    ///
    /// Returns a new `EpisodeBuilder` (builder pattern).
    #[napi]
    pub fn metadata(&self, m: serde_json::Value) -> napi::Result<EpisodeBuilder> {
        let map: serde_json::Map<String, serde_json::Value> = match m {
            serde_json::Value::Object(obj) => obj,
            _ => {
                return Err(napi::Error::from_reason(
                    "EpisodeBuilder.metadata: expected a JSON object",
                ));
            }
        };
        Ok(EpisodeBuilder { inner: self.inner.clone().metadata(map) })
    }
}

// ---------------------------------------------------------------------------
// NapiScopedLunaris
// ---------------------------------------------------------------------------

/// Scope-bound view over a `Lunaris` handle.
///
/// Constructed via `engine.scoped(scope)`. All operations issued through this
/// wrapper carry the bound scope as their partitioning key.
///
/// ```ts
/// import { Scope, EpisodeBuilder } from 'lunaris';
///
/// const scoped = engine.scoped(Scope.new("acme:agent-1"));
/// const lsn = await scoped.ingest(new EpisodeBuilder("src", "content"));
/// const hits = await scoped.recall("brown fox");
/// ```
///
/// ## Lifetime note
///
/// The TS-side wrapper owns `Arc<Lunaris>` so it is safe to use from async
/// contexts. On each method call the Rust side re-derives `ScopedLunaris<'_>`
/// via `inner.scoped(scope.clone())` — the borrow is local to the async block.
#[napi]
pub struct ScopedLunaris {
    pub(crate) inner: Arc<::lunaris::Lunaris>,
    pub(crate) scope: ::lunaris::Scope,
}

#[napi]
impl ScopedLunaris {
    /// Returns the scope this view is bound to.
    #[napi(getter)]
    pub fn scope(&self) -> Scope {
        Scope { inner: self.scope.clone() }
    }

    /// Ingest an episode payload under the bound scope.
    ///
    /// `builder` must be an `EpisodeBuilder`. The scope is stamped onto the
    /// episode by this method — callers cannot inject an arbitrary scope.
    ///
    /// Returns the string-formatted Lsn (e.g. `"1746000000000:1"`).
    #[napi]
    pub async fn ingest(&self, builder: &EpisodeBuilder) -> napi::Result<String> {
        let inner = self.inner.clone();
        let scope = self.scope.clone();
        let ep_builder = builder.inner.clone();
        let scoped = inner.scoped(scope);
        let lsn = scoped.ingest(ep_builder).await.map_err(napi_err)?;
        Ok(lsn.to_string())
    }

    /// Recall hits under the bound scope using a text query string.
    ///
    /// Returns a JSON array of hit objects. The scope is threaded through the
    /// entire retrieval path — only hits from this scope's partition are
    /// returned.
    #[napi]
    pub async fn recall(&self, query: String) -> napi::Result<serde_json::Value> {
        let inner = self.inner.clone();
        let scope = self.scope.clone();
        let scoped = inner.scoped(scope);
        let q = ::lunaris::Query::text(&query);
        let hits = scoped.recall(q).await.map_err(napi_err)?;
        serde_json::to_value(&hits).map_err(napi_err)
    }

    /// Return a `RetrievalBuilder` pre-seeded with this scope.
    ///
    /// Equivalent to `engine.recall().withScope(scope)` for DSL-style
    /// query composition.
    #[napi]
    pub fn dsl(&self) -> crate::generated::RetrievalBuilder {
        let builder = self.inner.scoped(self.scope.clone()).dsl();
        crate::generated::RetrievalBuilder { inner: Arc::new(builder) }
    }
}

// ---------------------------------------------------------------------------
// Free function: lunaris_scoped — factory exposed at crate root
// ---------------------------------------------------------------------------

/// Construct a `ScopedLunaris` bound to `scope`.
///
/// Exposed as a free function because napi-rs 3.x does not support adding
/// methods to a struct from outside its definition block. The TS-side
/// `index.mts` wrapper patches this as `Lunaris.prototype.scoped` so
/// callers can write `engine.scoped(scope)` naturally.
///
/// napi-rs exposes this as `lunarisScoped(handle, scope): ScopedLunaris`.
#[napi(js_name = "lunarisScoped")]
pub fn lunaris_scoped(handle: &Lunaris, scope: &Scope) -> ScopedLunaris {
    ScopedLunaris {
        inner: handle.inner.clone(),
        scope: scope.inner.clone(),
    }
}
