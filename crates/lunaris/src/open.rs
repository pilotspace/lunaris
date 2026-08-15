//! `lunaris::open(url)` — single entry point for the storage backend.
//!
//! Per `STORE-08`: `moon://host:port[?ws=workspace]` → `MoonStorage`. Since
//! 0.7.0 that is the **only** scheme: `postgres://`, `sqlite:///path` and
//! `memory://` were retired together with `lunaris-storage-postgres` /
//! `lunaris-storage-embedded`. All four of those spellings — plus anything
//! unrecognised — return
//! `LunarisError::Storage(StorageError::UnsupportedScheme(_))`; the retired
//! ones carry the migration instructions rather than a bare scheme name.
//!
//! The dispatcher returns `Arc<dyn StoragePort>` so the caller can hand the same
//! handle to multiple async tasks without re-opening the connection — and so the
//! handle survives across thread boundaries (`Send + Sync` is bound on the trait
//! and proven object-safe by `lunaris-core`'s `storage_trait_compiles` test).

use std::sync::Arc;

use lunaris_core::{LunarisError, StorageError, StoragePort};

/// The operator exit ramp printed by every retired-scheme error.
///
/// Kept as one constant so `open()` and `Lunaris::open` cannot drift into
/// telling an operator two different stories about the same URL.
pub(crate) const RETIRED_SCHEME_EXIT_RAMP: &str = "\
0.7.0 is Moon-only — use `moon://host:port`. \
Migrate your data with `lunaris-migrate` from the **v0.6.2 release binary** \
(it cannot be built from main; the crate was deleted with its backends), then \
re-open the store as `moon://`. See docs/migration/0.6-to-0.7.md for the \
procedure and the lossy-conversion contract, and \
docs/operations/external-moon.md to stand a Moon up.";

/// The retired-scheme error for `scheme`, or `None` if `scheme` was never one
/// of ours.
pub(crate) fn retired_scheme_error(scheme: &str) -> Option<StorageError> {
    let backend = match scheme {
        "postgres" | "postgresql" => "the Postgres backend (`lunaris-storage-postgres`)",
        "memory" | "sqlite" => "the embedded SQLite backend (`lunaris-storage-embedded`)",
        _ => return None,
    };
    Some(StorageError::UnsupportedScheme(format!(
        "`{scheme}://` was removed in 0.7.0 together with {backend}. {RETIRED_SCHEME_EXIT_RAMP}"
    )))
}

pub async fn open(url: &str) -> Result<Arc<dyn StoragePort>, LunarisError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        LunarisError::Storage(StorageError::UnsupportedScheme(format!("parse: {e}")))
    })?;
    match parsed.scheme() {
        "moon" => {
            let s = lunaris_storage_moon::MoonStorage::connect(url).await?;
            Ok(Arc::new(s) as Arc<dyn StoragePort>)
        }
        other => Err(LunarisError::Storage(
            retired_scheme_error(other)
                .unwrap_or_else(|| StorageError::UnsupportedScheme(other.into())),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every scheme 0.7.0 retired must fail with an error an operator can act
    /// on: the tool that moves the data, the release it ships in, and the
    /// document that explains what the move costs.
    #[tokio::test]
    async fn retired_schemes_name_the_exit_ramp() {
        for url in ["memory://", "sqlite:///tmp/lunaris.db", "postgres://u@h/db"] {
            let err = open(url).await.expect_err("retired scheme must not open");
            let LunarisError::Storage(StorageError::UnsupportedScheme(msg)) = err else {
                panic!("{url}: expected UnsupportedScheme, got {err:?}");
            };
            assert!(msg.contains("removed in 0.7.0"), "{url}: {msg}");
            assert!(msg.contains("lunaris-migrate"), "{url}: {msg}");
            assert!(msg.contains("v0.6.2"), "{url}: {msg}");
            assert!(msg.contains("docs/migration/0.6-to-0.7.md"), "{url}: {msg}");
            assert!(msg.contains("moon://"), "{url}: {msg}");
        }
    }

    #[tokio::test]
    async fn unknown_scheme_is_rejected() {
        assert!(matches!(
            open("mysql://localhost/db").await,
            Err(LunarisError::Storage(StorageError::UnsupportedScheme(_)))
        ));
    }

    /// A scheme we never served gets the plain error — the migration prose is
    /// reserved for URLs that used to work.
    #[test]
    fn foreign_scheme_gets_no_migration_prose() {
        assert!(retired_scheme_error("mysql").is_none());
    }
}
