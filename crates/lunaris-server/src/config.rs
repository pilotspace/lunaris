//! Plan 05-01 — clap-derive `Config` for `lunaris-server`.
//!
//! Every flag has a matching `LUNARIS_*` env var per CONTEXT.md D-26 12-factor
//! convention (Helios-style). CLI flags override env vars per clap default.

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "lunaris-server", version, about = "MemoryProtocol HTTP+SSE server")]
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
    use clap::Parser;

    #[test]
    fn config_parses_required_args() {
        let cfg = Config::try_parse_from([
            "lunaris-server",
            "--storage",
            "moon://localhost:6390",
            "--tokens-file",
            "/tmp/tokens.json",
        ])
        .expect("parse");
        assert_eq!(cfg.storage, "moon://localhost:6390");
        assert_eq!(cfg.bind, "0.0.0.0:8080");
        assert_eq!(cfg.rate_per_second, 60);
        assert_eq!(cfg.rate_burst, 120);
        assert_eq!(cfg.shutdown_grace_secs, 30);
        assert!(!cfg.metrics_disabled);
        assert_eq!(cfg.cors_origins, "*");
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
