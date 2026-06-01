//! Plan 05-01 — clap-derive `Config` for `lunaris-server`, plus the operational
//! subcommands (`migrate`, `bootstrap-db`) added for the onboarding overhaul.
//!
//! Every flag has a matching `LUNARIS_*` env var per CONTEXT.md D-26 12-factor
//! convention (Helios-style). CLI flags override env vars per clap default.
//!
//! ## Why two parsers instead of one
//!
//! The bare `lunaris-server --storage … --tokens-file …` form must keep
//! working (the conformance harness and the book both use it). clap 4.x's
//! `subcommand_negates_reqs` does **not** negate required args that arrive via
//! `#[command(flatten)]`, so a single `Cli { Option<Command> + flatten Config }`
//! struct would force `--storage` even for `migrate`. Instead `main` peeks at
//! `argv[1]`: a known subcommand → parse [`OpsCli`]; anything else → parse
//! [`Config`] directly (unchanged serve path).

use clap::{Parser, Subcommand};

/// Operational-subcommand entry point. Only constructed by `main` when
/// `argv[1]` is one of the [`Command`] names — never on the serve path.
#[derive(Parser, Debug)]
#[command(name = "lunaris-server", version, about = "Lunaris operational commands")]
pub struct OpsCli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Apply every embedded Postgres migration, then exit. Replaces the
    /// out-of-band `sqlx migrate run --source …` step — needs no `sqlx-cli`
    /// and no checked-out migrations directory. `--storage` must point at a
    /// DDL-capable role.
    Migrate {
        /// Admin (DDL-capable) Postgres URL.
        #[arg(long, env = "LUNARIS_STORAGE")]
        storage: String,
    },
    /// One-shot production bootstrap: run migrations as the admin role, then
    /// `CREATE ROLE <app-role> NOSUPERUSER NOBYPASSRLS LOGIN` (idempotent),
    /// grant it DML + sequence + schema usage, and report any tenant table
    /// missing `FORCE ROW LEVEL SECURITY` or write policy missing `WITH CHECK`.
    /// Replaces the hand-run §6.2 recipe in `docs/migration/0.1-to-0.2.md`.
    BootstrapDb {
        /// Admin (DDL-capable, can `CREATE ROLE`) Postgres URL.
        #[arg(long, env = "LUNARIS_ADMIN_URL")]
        admin_url: String,
        /// Name of the runtime application role to create/repair.
        #[arg(long, default_value = "lunaris_app")]
        app_role: String,
        /// Password for the application role (also via `LUNARIS_APP_PASSWORD`).
        #[arg(long, env = "LUNARIS_APP_PASSWORD")]
        app_password: String,
    },
}

impl Command {
    /// `true` if `name` is a recognised operational subcommand — used by `main`
    /// to decide which parser to run against `argv`.
    #[must_use]
    pub fn is_subcommand_name(name: &str) -> bool {
        matches!(name, "migrate" | "bootstrap-db")
    }
}

/// Serve-mode flags. The bare `lunaris-server …` form (no subcommand) parses
/// straight into these.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "lunaris-server",
    version,
    about = "MemoryProtocol HTTP+SSE server. Subcommands: `migrate`, `bootstrap-db` (see `lunaris-server migrate --help`)."
)]
pub struct Config {
    /// Bind address.
    #[arg(long, default_value = "0.0.0.0:8080", env = "LUNARIS_BIND")]
    pub bind: String,
    /// Storage URL (moon:// | postgres://) — required.
    #[arg(long, env = "LUNARIS_STORAGE")]
    pub storage: String,
    /// Bearer-token-map JSON file path (D-07).
    #[arg(long, env = "LUNARIS_TOKENS_FILE")]
    pub tokens_file: std::path::PathBuf,
    /// Per-tenant rate limit in requests/second (D-08).
    #[arg(long, default_value_t = 60, env = "LUNARIS_RATE_PER_SECOND")]
    pub rate_per_second: u32,
    /// Per-tenant burst budget (D-08).
    #[arg(long, default_value_t = 120, env = "LUNARIS_RATE_BURST")]
    pub rate_burst: u32,
    /// CORS allow-list (D-09); CSV, `*` for permissive.
    #[arg(long, default_value = "*", env = "LUNARIS_CORS_ORIGINS")]
    pub cors_origins: String,
    /// Graceful shutdown drain window in seconds.
    #[arg(long, default_value_t = 30, env = "LUNARIS_SHUTDOWN_GRACE_SECS")]
    pub shutdown_grace_secs: u64,
    /// Disable /metrics endpoint (Plan 05-05 will gate metrics layer on this).
    #[arg(long)]
    pub metrics_disabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_required_args() {
        let cfg = Config::try_parse_from([
            "lunaris-server",
            "--storage",
            "moon://localhost:6380",
            "--tokens-file",
            "/tmp/tokens.json",
        ])
        .expect("parse");
        assert_eq!(cfg.storage, "moon://localhost:6380");
        assert_eq!(cfg.bind, "0.0.0.0:8080");
        assert_eq!(cfg.rate_per_second, 60);
        assert_eq!(cfg.rate_burst, 120);
        assert_eq!(cfg.shutdown_grace_secs, 30);
        assert!(!cfg.metrics_disabled);
        assert_eq!(cfg.cors_origins, "*");
    }

    #[test]
    fn subcommand_name_detection() {
        assert!(Command::is_subcommand_name("migrate"));
        assert!(Command::is_subcommand_name("bootstrap-db"));
        assert!(!Command::is_subcommand_name("--storage"));
        assert!(!Command::is_subcommand_name("serve"));
    }

    #[test]
    fn ops_cli_parses_migrate() {
        let cli = OpsCli::try_parse_from([
            "lunaris-server",
            "migrate",
            "--storage",
            "postgres://admin:pw@localhost/lunaris",
        ])
        .expect("parse migrate");
        match cli.command {
            Command::Migrate { storage } => {
                assert_eq!(storage, "postgres://admin:pw@localhost/lunaris");
            }
            other => panic!("expected Migrate, got {other:?}"),
        }
    }

    #[test]
    fn ops_cli_parses_bootstrap_db_with_role_default() {
        let cli = OpsCli::try_parse_from([
            "lunaris-server",
            "bootstrap-db",
            "--admin-url",
            "postgres://admin@localhost/lunaris",
            "--app-password",
            "s3cret",
        ])
        .expect("parse bootstrap-db");
        match cli.command {
            Command::BootstrapDb { admin_url, app_role, app_password } => {
                assert_eq!(admin_url, "postgres://admin@localhost/lunaris");
                assert_eq!(app_role, "lunaris_app");
                assert_eq!(app_password, "s3cret");
            }
            other => panic!("expected BootstrapDb, got {other:?}"),
        }
    }

    #[test]
    fn config_overrides_via_cli() {
        let cfg = Config::try_parse_from([
            "lunaris-server",
            "--storage",
            "postgres://localhost/lunaris",
            "--tokens-file",
            "/etc/lunaris/tokens.json",
            "--bind",
            "127.0.0.1:9090",
            "--rate-per-second",
            "120",
            "--rate-burst",
            "240",
            "--cors-origins",
            "https://a.com,https://b.com",
            "--shutdown-grace-secs",
            "10",
            "--metrics-disabled",
        ])
        .expect("parse");
        assert_eq!(cfg.bind, "127.0.0.1:9090");
        assert_eq!(cfg.rate_per_second, 120);
        assert_eq!(cfg.rate_burst, 240);
        assert_eq!(cfg.cors_origins, "https://a.com,https://b.com");
        assert_eq!(cfg.shutdown_grace_secs, 10);
        assert!(cfg.metrics_disabled);
    }
}
