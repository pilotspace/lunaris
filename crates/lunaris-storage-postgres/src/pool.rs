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

    /// Apply every embedded migration at `url`. The role behind `url` must be
    /// DDL-capable (an owner / `NOSUPERUSER` admin role) — this is the
    /// privileged half of the "admin migrates, app role runs" split, replacing
    /// the out-of-band `sqlx migrate run --source crates/.../migrations` step.
    ///
    /// Idempotent: sqlx records applied versions in `_sqlx_migrations` and skips
    /// them on re-run. The pool is opened with a tiny `max_connections` and
    /// closed before returning so this can be called as a one-shot.
    pub async fn migrate(url: &str) -> Result<(), StorageError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| StorageError::UnsupportedScheme(format!("postgres parse: {e}")))?;
        match parsed.scheme() {
            "postgres" | "postgresql" => {}
            other => return Err(StorageError::UnsupportedScheme(other.into())),
        }
        let pool = PgPoolOptions::new().max_connections(2).connect(url).await.map_err(sqlx_err)?;
        let res = sqlx::migrate!("./migrations").run(&pool).await.map_err(migrate_err);
        pool.close().await;
        res
    }

    /// Read-only check (safe for a non-DDL app role): are all embedded
    /// migrations already applied at `url`? `false` when the schema is missing
    /// or behind; used by [`crate::PostgresStorage::connect_or_hint`] to turn a
    /// permission error into an actionable "run `lunaris-server migrate`" hint.
    pub async fn schema_is_current(url: &str) -> Result<bool, StorageError> {
        let pool = PgPoolOptions::new().max_connections(1).connect(url).await.map_err(sqlx_err)?;
        let want_max: i64 =
            sqlx::migrate!("./migrations").iter().map(|m| m.version).max().unwrap_or(0);
        // `_sqlx_migrations` may not exist yet — treat any read error as "behind".
        let have_max: Option<i64> =
            sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
                .fetch_one(&pool)
                .await
                .ok()
                .flatten();
        pool.close().await;
        Ok(have_max == Some(want_max))
    }

    /// Connect without running migrations. Used by non-privileged application roles
    /// (e.g., `lunaris_app`) that have DML but not DDL access. Migrations MUST be
    /// applied by a privileged connection first.
    pub async fn connect_no_migrate(url: &str) -> Result<Self, StorageError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| StorageError::UnsupportedScheme(format!("postgres parse: {e}")))?;
        match parsed.scheme() {
            "postgres" | "postgresql" => {}
            other => return Err(StorageError::UnsupportedScheme(other.into())),
        }
        let pool = PgPoolOptions::new().max_connections(8).connect(url).await.map_err(sqlx_err)?;

        // Per-session AGE bootstrap — best-effort, same as connect().
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
