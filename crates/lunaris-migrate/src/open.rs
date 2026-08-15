//! URL → `StoragePort` for the two ends of the migration.
//!
//! Kept in the library rather than the binary so the destination's index-width
//! contract is testable against a real Moon (see `tests/dest_index_dim.rs`).

use std::sync::Arc;

use lunaris_core::StoragePort;

/// Failure modes of opening either end.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The scheme is not one this tool accepts for that end.
    #[error("{0}")]
    Scheme(String),
    /// The backend refused the connection.
    #[error("open {url}: {source}")]
    Backend { url: String, source: lunaris_core::StorageError },
}

/// Open the migration SOURCE: SQLite (`sqlite://` / `memory://`) or Postgres.
///
/// Postgres opens with `connect_no_migrate` on purpose — a migration source is
/// read-only, and running DDL against the store an operator is leaving is not
/// this tool's business.
pub async fn open_source(url: &str) -> Result<Arc<dyn StoragePort>, OpenError> {
    let scheme = url.split(':').next().unwrap_or_default();
    let backend = |source| OpenError::Backend { url: url.to_owned(), source };
    match scheme {
        "memory" | "sqlite" => {
            let s =
                lunaris_storage_embedded::EmbeddedStorage::connect(url).await.map_err(backend)?;
            Ok(Arc::new(s))
        }
        "postgres" | "postgresql" => {
            let s = lunaris_storage_postgres::PostgresStorage::connect_no_migrate(url)
                .await
                .map_err(backend)?;
            Ok(Arc::new(s))
        }
        "moon" => {
            Err(OpenError::Scheme("Moon is the destination of this tool, not a source".into()))
        }
        other => Err(OpenError::Scheme(format!(
            "unsupported source scheme {other:?}: use sqlite://, memory://, postgres://"
        ))),
    }
}

/// Open the migration DESTINATION. Moon only — this tool has one direction.
///
/// `dim` is load-bearing even though the migration writes no vectors: opening a
/// Moon handle CREATES the `chunks` / `entities` / `facts` / `communities` FT
/// indices when they are absent, and `FT.CREATE`'s `DIM` is sticky. Connecting
/// at the wrong width leaves a destination whose indices can never accept the
/// operator's embeddings without `FT.DROPINDEX` + a full re-ingest — which is
/// also why passing it through (rather than defaulting inside) is the whole
/// point of the parameter.
///
/// The connect path is the ordinary `MoonStorage::connect_with_dim`, so the
/// multi-shard guard and the server-version handshake both run and there is
/// deliberately no way to bypass them.
pub async fn open_dest(url: &str, dim: usize) -> Result<Arc<dyn StoragePort>, OpenError> {
    if !url.starts_with("moon://") {
        return Err(OpenError::Scheme(
            "destination must be a moon:// URL (this tool migrates INTO Moon)".into(),
        ));
    }
    // RED: `dim` is accepted and ignored — the destination's indices get
    // created at the built-in default width.
    let _ = dim;
    let s = lunaris_storage_moon::MoonStorage::connect(url)
        .await
        .map_err(|source| OpenError::Backend { url: url.to_owned(), source })?;
    Ok(Arc::new(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn moon_is_rejected_as_a_source() {
        let e = open_source("moon://127.0.0.1:1").await.expect_err("moon is not a source");
        assert!(matches!(e, OpenError::Scheme(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn an_unknown_source_scheme_is_rejected_before_any_io() {
        let e = open_source("mysql://localhost/db").await.expect_err("unsupported scheme");
        assert!(e.to_string().contains("mysql"), "error must echo the scheme: {e}");
    }

    #[tokio::test]
    async fn a_non_moon_destination_is_rejected() {
        let e = open_dest("sqlite:///tmp/x.db", 768).await.expect_err("dest must be moon");
        assert!(matches!(e, OpenError::Scheme(_)), "got {e:?}");
    }
}
