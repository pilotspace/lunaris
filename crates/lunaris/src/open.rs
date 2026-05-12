//! `lunaris::open(url)` — single entry point for both backends.
//!
//! Per `STORE-08`: `moon://host:port[?ws=workspace]` → `MoonStorage`,
//! `postgres://user:pass@host/db` → `PostgresStorage`,
//! `memory://` / `sqlite:///path` → `EmbeddedStorage` (the zero-dependency
//! dev/embedded backend). Anything else returns
//! `LunarisError::Storage(StorageError::UnsupportedScheme(_))`.
//!
//! The dispatcher returns `Arc<dyn StoragePort>` so the caller can hand the same
//! handle to multiple async tasks without re-opening the connection — and so the
//! handle survives across thread boundaries (`Send + Sync` is bound on the trait
//! and proven object-safe by `lunaris-core`'s `storage_trait_compiles` test).

use std::sync::Arc;

use lunaris_core::{LunarisError, StorageError, StoragePort};

pub async fn open(url: &str) -> Result<Arc<dyn StoragePort>, LunarisError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        LunarisError::Storage(StorageError::UnsupportedScheme(format!("parse: {e}")))
    })?;
    match parsed.scheme() {
        "moon" => {
            let s = lunaris_storage_moon::MoonStorage::connect(url).await?;
            Ok(Arc::new(s) as Arc<dyn StoragePort>)
        }
        "postgres" | "postgresql" => {
            // Mirror `Lunaris::open`: honour `LUNARIS_ADMIN_URL` so migrations
            // run over a DDL-capable admin connection and the runtime handle
            // binds to a (possibly non-DDL) app role — no out-of-band
            // `sqlx migrate run` step.
            let admin_url =
                std::env::var("LUNARIS_ADMIN_URL").ok().filter(|s| !s.trim().is_empty());
            let s = lunaris_storage_postgres::PostgresStorage::connect_with_admin(
                url,
                admin_url.as_deref(),
            )
            .await?;
            Ok(Arc::new(s) as Arc<dyn StoragePort>)
        }
        "memory" | "sqlite" => {
            let s = lunaris_storage_embedded::EmbeddedStorage::connect(url).await?;
            Ok(Arc::new(s) as Arc<dyn StoragePort>)
        }
        other => Err(LunarisError::Storage(StorageError::UnsupportedScheme(other.into()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_scheme_opens_embedded_backend() {
        let s = open("memory://").await.expect("memory:// opens");
        let caps = s.capabilities();
        assert!(caps.bi_temporal_native);
        assert!(!caps.graph_native);
        assert!(!caps.queue_native);
    }

    #[tokio::test]
    async fn unknown_scheme_is_rejected() {
        assert!(matches!(
            open("mysql://localhost/db").await,
            Err(LunarisError::Storage(StorageError::UnsupportedScheme(_)))
        ));
    }
}
