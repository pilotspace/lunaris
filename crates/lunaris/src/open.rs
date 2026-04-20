//! `lunaris::open(url)` — single entry point for both backends.
//!
//! Per `STORE-08`: `moon://host:port[?ws=workspace]` → `MoonStorage`,
//! `postgres://user:pass@host/db` → `PostgresStorage`. Anything else returns
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
            let s = lunaris_storage_postgres::PostgresStorage::connect(url).await?;
            Ok(Arc::new(s) as Arc<dyn StoragePort>)
        }
        other => Err(LunarisError::Storage(StorageError::UnsupportedScheme(other.into()))),
    }
}
