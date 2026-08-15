//! Plan 05-01 — clap-derive `Config` for `lunaris-server`.
//!
//! Every flag has a matching `LUNARIS_*` env var per CONTEXT.md D-26 12-factor
//! convention (Helios-style). CLI flags override env vars per clap default.
//!
//! ## The retired operational subcommands
//!
//! Through 0.6.x this module also carried `migrate` and `bootstrap-db` — both
//! pure Postgres operations (apply the embedded `sqlx` migration set; create
//! the `NOSUPERUSER NOBYPASSRLS` app role). 0.7.0 is Moon-only and deleted
//! `lunaris-storage-postgres`, so neither has anything left to act on: Moon
//! needs no schema migration and no role bootstrap.
//!
//! They are NOT silently gone. [`retired_subcommand`] still recognises both
//! names so `main` can fail with the migration story rather than clap's
//! "unexpected argument" — an operator whose runbook still says
//! `lunaris-server migrate` gets told where the data goes, not that they
//! mistyped. With the subcommands went the two-parser dance: there is one
//! parser again, and it is [`Config`].

use clap::Parser;

/// The operator-facing message for a subcommand 0.7.0 retired, or `None` if
/// `name` was never one of ours.
#[must_use]
pub fn retired_subcommand(name: &str) -> Option<String> {
    let what = match name {
        "migrate" => "applied the embedded Postgres migration set",
        "bootstrap-db" => "created the Postgres NOSUPERUSER NOBYPASSRLS app role",
        _ => return None,
    };
    Some(format!(
        "`lunaris-server {name}` {what} and was removed in 0.7.0 together with the \
         Postgres backend. Moon needs neither a schema migration nor a role bootstrap: \
         point `--storage` at `moon://host:port` and serve. To move existing \
         Postgres/SQLite data into Moon, run `lunaris-migrate` from the v0.6.2 release \
         binary BEFORE upgrading — see docs/migration/0.6-to-0.7.md, and \
         docs/operations/external-moon.md to stand a Moon up."
    ))
}

/// Serve-mode flags. `lunaris-server …` parses straight into these.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "lunaris-server",
    version,
    about = "MemoryProtocol HTTP+SSE server (Moon-only storage since 0.7.0)."
)]
pub struct Config {
    /// Bind address.
    #[arg(long, default_value = "0.0.0.0:8080", env = "LUNARIS_BIND")]
    pub bind: String,
    /// Storage URL (`moon://host:port`) — required. 0.7.0 is Moon-only.
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
    /// 0.6.2 P0-2 — per-request wall-clock budget in seconds. A request that
    /// exceeds it is cut off with `408 Request Timeout`. `0` disables the
    /// timeout entirely (for operators who bound requests at their proxy).
    ///
    /// The budget covers producing the RESPONSE, not streaming its body, so an
    /// SSE `/v1/recall` stream is not severed mid-flight.
    #[arg(long, default_value_t = 30, env = "LUNARIS_HTTP_TIMEOUT_SECS")]
    pub http_timeout_secs: u64,
    /// 0.6.2 P0-2 — maximum number of requests being served concurrently.
    /// Arrivals beyond the cap are SHED immediately (`503` + `Retry-After`)
    /// rather than queued, so a slow backend cannot convert into unbounded
    /// in-flight work. `0` disables the limit.
    #[arg(long, default_value_t = 256, env = "LUNARIS_HTTP_CONCURRENCY")]
    pub http_concurrency: usize,
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
        assert_eq!(cfg.http_timeout_secs, 30, "0.6.2 P0-2 default request budget");
        assert_eq!(cfg.http_concurrency, 256, "0.6.2 P0-2 default concurrency cap");
    }

    /// The two Postgres-era subcommands must still be RECOGNISED after their
    /// removal, so an operator running a stale runbook is handed the migration
    /// path instead of a clap parse error.
    #[test]
    fn retired_subcommands_name_the_exit_ramp() {
        for name in ["migrate", "bootstrap-db"] {
            let msg = retired_subcommand(name).unwrap_or_else(|| panic!("{name} must be known"));
            assert!(msg.contains("removed in 0.7.0"), "{name}: {msg}");
            assert!(msg.contains("lunaris-migrate"), "{name}: {msg}");
            assert!(msg.contains("v0.6.2"), "{name}: {msg}");
            assert!(msg.contains("docs/migration/0.6-to-0.7.md"), "{name}: {msg}");
            assert!(msg.contains("moon://"), "{name}: {msg}");
        }
    }

    #[test]
    fn serve_flags_are_not_mistaken_for_subcommands() {
        assert!(retired_subcommand("--storage").is_none());
        assert!(retired_subcommand("serve").is_none());
    }

    #[test]
    fn config_overrides_via_cli() {
        let cfg = Config::try_parse_from([
            "lunaris-server",
            "--storage",
            "moon://db.internal:6380",
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
            "--http-timeout-secs",
            "5",
            "--http-concurrency",
            "32",
            "--metrics-disabled",
        ])
        .expect("parse");
        assert_eq!(cfg.bind, "127.0.0.1:9090");
        assert_eq!(cfg.rate_per_second, 120);
        assert_eq!(cfg.rate_burst, 240);
        assert_eq!(cfg.cors_origins, "https://a.com,https://b.com");
        assert_eq!(cfg.shutdown_grace_secs, 10);
        assert_eq!(cfg.http_timeout_secs, 5);
        assert_eq!(cfg.http_concurrency, 32);
        assert!(cfg.metrics_disabled);
    }
}
