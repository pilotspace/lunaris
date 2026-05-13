//! One-shot production database bootstrap — the in-process replacement for the
//! hand-run "§6.2 recipe" in `docs/migration/0.1-to-0.2.md`.
//!
//! [`bootstrap_app_role`] connects as a DDL-capable admin role and, in order:
//!
//! 1. applies every embedded migration ([`crate::PgClient::migrate`]);
//! 2. creates (or repairs) a `NOSUPERUSER NOBYPASSRLS LOGIN` application role
//!    — the role RLS actually applies to (superusers / `BYPASSRLS` roles skip
//!    every policy regardless of `FORCE ROW LEVEL SECURITY`);
//! 3. grants it `USAGE` on `public`, DML on all tables, `USAGE` on all
//!    sequences, plus matching `ALTER DEFAULT PRIVILEGES` for future objects;
//! 4. returns a [`BootstrapReport`] listing any tenant table missing
//!    `FORCE ROW LEVEL SECURITY` and any `tenant_isolation`-style policy that
//!    lacks a `WITH CHECK` clause (read-tight but INSERT-leaky — RC-3).
//!
//! Role/identifier injection is closed two ways: the role name is validated
//! against `^[A-Za-z_][A-Za-z0-9_]{0,62}$` in Rust before it ever reaches SQL,
//! and the dynamic DDL is built server-side with `format('… %I … %L …')` so
//! both the identifier and the password literal are quoted by Postgres.

use lunaris_core::error::StorageError;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;

use crate::pool::{PgClient, sqlx_err};

/// Result of a [`bootstrap_app_role`] run — empty vectors mean "nothing to fix".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// Tenant tables with RLS enabled but `FORCE ROW LEVEL SECURITY` off.
    pub tables_missing_force_rls: Vec<String>,
    /// `(policyname, tablename)` pairs whose write-capable policy has no
    /// `WITH CHECK` expression.
    pub policies_missing_with_check: Vec<(String, String)>,
}

impl BootstrapReport {
    /// `true` when both findings lists are empty — the database is correctly
    /// hardened for a non-superuser app role.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.tables_missing_force_rls.is_empty() && self.policies_missing_with_check.is_empty()
    }
}

/// Validate a Postgres role identifier we are about to interpolate into DDL.
fn valid_role_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    name.len() <= 63 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// See module docs. `admin_url` must be a DDL-capable role that can also
/// `CREATE ROLE`. `app_role` is validated; `app_password` is passed as a bind
/// value and quoted server-side via `format('… %L', …)`.
pub async fn bootstrap_app_role(
    admin_url: &str,
    app_role: &str,
    app_password: &str,
) -> Result<BootstrapReport, StorageError> {
    if !valid_role_name(app_role) {
        return Err(StorageError::Backend(format!(
            "invalid app role name {app_role:?}: must match ^[A-Za-z_][A-Za-z0-9_]{{0,62}}$"
        )));
    }

    // 1. Migrations first — over the admin connection.
    PgClient::migrate(admin_url).await?;

    let pool =
        PgPoolOptions::new().max_connections(2).connect(admin_url).await.map_err(sqlx_err)?;

    // 2 + 3. Create/repair the role and grant it the runtime privileges.
    // The custom GUCs `lunaris.bootstrap_role` / `lunaris.bootstrap_pw` carry
    // the bind values into the DO block (PL/pgSQL DO blocks cannot take params).
    sqlx::query("SELECT set_config('lunaris.bootstrap_role', $1, false)")
        .bind(app_role)
        .execute(&pool)
        .await
        .map_err(sqlx_err)?;
    sqlx::query("SELECT set_config('lunaris.bootstrap_pw', $1, false)")
        .bind(app_password)
        .execute(&pool)
        .await
        .map_err(sqlx_err)?;
    sqlx::query(BOOTSTRAP_ROLE_DO).execute(&pool).await.map_err(sqlx_err)?;
    // Don't leave the password sitting in a session GUC.
    let _ =
        sqlx::query("SELECT set_config('lunaris.bootstrap_pw', '', false)").execute(&pool).await;

    // 4. Hardening report.
    let force_rows = sqlx::query(
        "SELECT c.relname AS name \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p') \
           AND c.relrowsecurity AND NOT c.relforcerowsecurity \
         ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .map_err(sqlx_err)?;
    let tables_missing_force_rls = force_rows.iter().map(|r| r.get::<String, _>("name")).collect();

    let policy_rows = sqlx::query(
        "SELECT policyname, tablename FROM pg_policies \
         WHERE schemaname = 'public' AND with_check IS NULL \
           AND cmd IN ('ALL', 'INSERT', 'UPDATE') \
         ORDER BY tablename, policyname",
    )
    .fetch_all(&pool)
    .await
    .map_err(sqlx_err)?;
    let policies_missing_with_check = policy_rows
        .iter()
        .map(|r| (r.get::<String, _>("policyname"), r.get::<String, _>("tablename")))
        .collect();

    pool.close().await;
    Ok(BootstrapReport { tables_missing_force_rls, policies_missing_with_check })
}

const BOOTSTRAP_ROLE_DO: &str = r#"
DO $bootstrap$
DECLARE
  r  text := current_setting('lunaris.bootstrap_role');
  pw text := current_setting('lunaris.bootstrap_pw');
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r) THEN
    EXECUTE format('ALTER ROLE %I WITH LOGIN PASSWORD %L NOSUPERUSER NOBYPASSRLS', r, pw);
  ELSE
    EXECUTE format('CREATE ROLE %I WITH LOGIN PASSWORD %L NOSUPERUSER NOBYPASSRLS', r, pw);
  END IF;
  EXECUTE format('GRANT USAGE ON SCHEMA public TO %I', r);
  EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO %I', r);
  EXECUTE format('GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO %I', r);
  EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO %I', r);
  EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE ON SEQUENCES TO %I', r);
END
$bootstrap$;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_name_validation() {
        assert!(valid_role_name("lunaris_app"));
        assert!(valid_role_name("_app2"));
        assert!(valid_role_name("App"));
        assert!(!valid_role_name(""));
        assert!(!valid_role_name("1app"));
        assert!(!valid_role_name("app-role"));
        assert!(!valid_role_name("app;DROP ROLE x"));
        assert!(!valid_role_name("app role"));
        assert!(!valid_role_name(&"a".repeat(64)));
        assert!(valid_role_name(&"a".repeat(63)));
    }

    #[test]
    fn report_is_clean_when_empty() {
        assert!(BootstrapReport::default().is_clean());
        let dirty = BootstrapReport {
            tables_missing_force_rls: vec!["facts".into()],
            ..Default::default()
        };
        assert!(!dirty.is_clean());
    }

    #[tokio::test]
    async fn rejects_bad_role_before_connecting() {
        // No live Postgres needed — validation happens before any I/O.
        let r = bootstrap_app_role("postgres://nope/db", "bad;role", "pw").await;
        assert!(matches!(r, Err(StorageError::Backend(_))));
    }
}
