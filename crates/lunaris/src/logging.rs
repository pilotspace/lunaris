//! Plan 05-05 OPS-08 — structured logging defaults to JSON in production,
//! pretty in dev (CONTEXT.md D-26 verbatim).
//!
//! ## Mode selection (D-26)
//!
//! - `LUNARIS_ENV=production` OR `!std::io::stdout().is_terminal()` → JSON
//!   formatter via `tracing_subscriber::fmt().json().with_current_span(true)
//!   .with_span_list(true)`.
//! - Otherwise → pretty formatter via `tracing_subscriber::fmt().pretty()`.
//!
//! ## Idempotency
//!
//! Uses `try_init().ok()` so test code with its own subscriber doesn't panic
//! on duplicate-init. Calling [`init`] multiple times is safe — subsequent
//! calls become no-ops once a global subscriber is installed.
//!
//! ## Public callers
//!
//! - `lunaris-server::main` (REPLACES the Plan 05-01 minimal `tracing_subscriber::fmt()` init).
//! - Embedded callers may invoke `lunaris::init_logging()` (re-export) at process
//!   start to opt in to the same JSON / pretty selector.

#![forbid(unsafe_code)]

use is_terminal::IsTerminal;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

/// Env var that flips the formatter to JSON unconditionally per CONTEXT.md D-26.
const LUNARIS_ENV: &str = "LUNARIS_ENV";
/// Value treated as "production" for the [`LUNARIS_ENV`] check.
const PRODUCTION: &str = "production";

/// Initialize the global tracing subscriber.
///
/// Picks JSON vs pretty per CONTEXT.md D-26:
/// - `LUNARIS_ENV=production` → JSON.
/// - stdout is NOT a TTY (e.g., piped, systemd journal, container log) → JSON.
/// - else → pretty.
///
/// Honors `RUST_LOG` (or any `EnvFilter::try_from_default_env`-recognized var)
/// for level filtering; defaults to `info` when unset.
///
/// Idempotent — `try_init` swallows the duplicate-init error so tests / other
/// callers that already installed a subscriber are not disrupted.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let prod = std::env::var(LUNARIS_ENV).as_deref() == Ok(PRODUCTION)
        || !std::io::stdout().is_terminal();

    if prod {
        let _ = fmt()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .with_env_filter(filter)
            .try_init();
    } else {
        let _ = fmt().pretty().with_env_filter(filter).try_init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        // First call MAY install a subscriber; subsequent calls MUST be no-ops
        // (try_init swallows the duplicate-init error). The test passes iff
        // none of the three calls panic.
        init();
        init();
        init();
    }

    #[test]
    fn lunaris_env_constant_matches_d26() {
        assert_eq!(LUNARIS_ENV, "LUNARIS_ENV");
        assert_eq!(PRODUCTION, "production");
    }
}
