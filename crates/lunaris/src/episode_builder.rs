//! [`EpisodeBuilder`] — scope-less payload builder for [`lunaris_core::Episode`].
//!
//! Lives in the `lunaris` crate (NOT `lunaris-core`) so that
//! `EpisodeBuilder::into_episode` can be `pub(crate)`. Placing the method
//! here means only code inside the `lunaris` crate (i.e., only
//! [`crate::handle::ScopedLunaris::ingest`]) can call `into_episode` — callers
//! outside the crate cannot stamp an arbitrary scope onto an episode.
//!
//! ## Visibility contract
//!
//! | Symbol                       | Visibility    | Rationale                                  |
//! |------------------------------|---------------|--------------------------------------------|
//! | `EpisodeBuilder`             | `pub`         | Callers need to construct + configure it   |
//! | `EpisodeBuilder::new`        | `pub`         | Public constructor                         |
//! | `EpisodeBuilder::t_ref`      | `pub`         | Builder-pattern setter                     |
//! | `EpisodeBuilder::metadata`   | `pub`         | Builder-pattern setter                     |
//! | `EpisodeBuilder::into_episode` | `pub(crate)` | Only `ScopedLunaris::ingest` may call this |
//! | struct fields                | private       | Enforce construction only via `new()`      |

use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::{Episode, HlcClock, Scope};
use ulid::Ulid;

/// Scope-less payload builder for [`Episode`].
///
/// Callers assemble all Episode fields **except** scope using this builder.
/// Scope is injected exactly once — by [`crate::handle::ScopedLunaris::ingest`]
/// — via the `pub(crate)` `EpisodeBuilder::into_episode` method. This makes it
/// impossible to construct an [`Episode`] with an arbitrary scope by reaching
/// around the `ScopedLunaris` wrapper.
///
/// # Example
///
/// ```ignore
/// let builder = EpisodeBuilder::new("agent:fs/report.md", "# Q3 Report\n...")
///     .t_ref(chrono::Utc::now());
/// // scope is injected by engine.scoped(scope_a).ingest(builder).await?
/// ```
#[derive(Clone, Debug)]
#[must_use = "EpisodeBuilder is consumed by ScopedLunaris::ingest; constructing it without calling .ingest() is a no-op"]
pub struct EpisodeBuilder {
    id: Option<Ulid>,
    source: String,
    content: String,
    t_ref: Option<chrono::DateTime<chrono::Utc>>,
    metadata: serde_json::Map<String, serde_json::Value>,
}

impl EpisodeBuilder {
    /// Construct a new builder with the required `source` and `content`.
    ///
    /// `source` is the namespace-qualified origin identifier
    /// (e.g. `"helios:fs/report.md"` or `"chat:session-42/turn-7"`).
    /// `content` is the raw text that will be chunked + embedded.
    pub fn new(source: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: None,
            source: source.into(),
            content: content.into(),
            t_ref: None,
            metadata: serde_json::Map::new(),
        }
    }

    /// Override the auto-generated ULID with a deterministic `id`.
    ///
    /// Useful for idempotent ingest (replay / migration tooling). When not
    /// set, `into_episode` generates a fresh ULID via [`Ulid::new`].
    pub fn id(mut self, id: Ulid) -> Self {
        self.id = Some(id);
        self
    }

    /// Set the reference timestamp (valid time anchor).
    ///
    /// When not set, the ingest pipeline uses the current wall time from the
    /// [`HlcClock`] bound to the engine.
    pub fn t_ref(mut self, t: chrono::DateTime<chrono::Utc>) -> Self {
        self.t_ref = Some(t);
        self
    }

    /// Merge `metadata` key/value pairs into the builder.
    pub fn metadata(mut self, m: serde_json::Map<String, serde_json::Value>) -> Self {
        self.metadata.extend(m);
        self
    }

    /// Materialise the builder into an [`Episode`].
    ///
    /// `pub(crate)` — only [`crate::handle::ScopedLunaris::ingest`] may call
    /// this. Callers outside the `lunaris` crate cannot set an arbitrary scope
    /// on an episode.
    ///
    /// The `clock` is the engine's [`HlcClock`]; [`BiTemporal::now`] stamps
    /// the bi-temporal `(valid, sys)` pair at the moment of ingest.
    pub(crate) fn into_episode(self, scope: Scope, clock: &HlcClock) -> Episode {
        Episode {
            // No `.id(...)` override → a fresh ULID. `unwrap_or_default()`
            // would hand back `Ulid(0)` for *every* builder-built episode,
            // so two ingests under the same scope would collide on
            // `episode_key(scope, Ulid(0))` and the second would silently
            // overwrite the first.
            // Must use `Ulid::new` (random) not `Ulid::default()` (which is `Ulid(0)`).
            #[allow(clippy::unwrap_or_default)]
            id: self.id.unwrap_or_else(Ulid::new),
            scope,
            source: self.source,
            content: self.content,
            t_ref: self.t_ref,
            bt: BiTemporal::now(clock),
            metadata: self.metadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_builders_get_distinct_ids() {
        let clock = HlcClock::new(0);
        let scope = Scope::dev();
        let a = EpisodeBuilder::new("s", "a").into_episode(scope.clone(), &clock);
        let b = EpisodeBuilder::new("s", "b").into_episode(scope, &clock);
        assert_ne!(a.id, b.id, "auto-generated episode ids must be unique");
        assert_ne!(a.id, Ulid::nil());
    }

    #[test]
    fn explicit_id_is_preserved() {
        let clock = HlcClock::new(0);
        let id = Ulid::new();
        let ep = EpisodeBuilder::new("s", "c").id(id).into_episode(Scope::dev(), &clock);
        assert_eq!(ep.id, id);
    }
}
