//! Connection pool for Postgres — wraps `sqlx::postgres::PgPool` and runs migrations on connect.
//!
//! ## Threat note (T-01-04-04)
//!
//! `PgClient` derives `Debug` here for dev ergonomics. The `url` field can carry an
//! embedded password (`postgres://user:pass@host/db`) and would leak in any log/print
//! that formats the struct. Phase 4 (`OPS-05`) wires the tracing layer and at that point
//! we hand-implement `Debug` to redact `userinfo`. v0 does not log this struct, so the
//! follow-up is recorded in STATE.md instead of being a Phase 1 blocker.

use lunaris_core::error::StorageError;
use sqlx::postgres::{PgPool, PgPoolOptions};

#[derive(Debug, Clone)]
pub struct PgClient {
    pub url: String,
    pub pool: PgPool,
}

impl PgClient {
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| StorageError::UnsupportedScheme(format!("postgres parse: {e}")))?;
        match parsed.scheme() {
            "postgres" | "postgresql" => {}
            other => return Err(StorageError::UnsupportedScheme(other.into())),
        }
        let pool = PgPoolOptions::new().max_connections(8).connect(url).await.map_err(sqlx_err)?;

        // Run migrations. sqlx::migrate! looks up files at compile time relative to the crate root.
        sqlx::migrate!("./migrations").run(&pool).await.map_err(migrate_err)?;

        // Per-session AGE bootstrap (LOAD + search_path) — required for cypher() function visibility.
        // Best-effort: ignore failures so a Postgres-without-AGE instance still boots far enough
        // for `cargo check`.
        let _ = sqlx::query("LOAD 'age'").execute(&pool).await;
        let _ = sqlx::query("SET search_path = ag_catalog, \"$user\", public").execute(&pool).await;

        Ok(Self { url: url.to_string(), pool })
    }
}

#[inline]
pub fn sqlx_err(e: sqlx::Error) -> StorageError {
    StorageError::Backend(format!("postgres: {e}"))
}

#[inline]
pub fn migrate_err(e: sqlx::migrate::MigrateError) -> StorageError {
    StorageError::Backend(format!("postgres migrate: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::error::StorageError;

    #[tokio::test]
    async fn rejects_wrong_scheme() {
        let r = PgClient::connect("mysql://localhost/lunaris").await;
        assert!(matches!(r, Err(StorageError::UnsupportedScheme(_))));
    }

    #[tokio::test]
    async fn rejects_garbage_url() {
        let r = PgClient::connect("not a url").await;
        assert!(matches!(r, Err(StorageError::UnsupportedScheme(_))));
    }
}
