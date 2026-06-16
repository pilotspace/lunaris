//! Memory Inspector (Phase 1) — the served dashboard shell.
//!
//! `GET /` (PUBLIC, root router) serves the single-file read-only Inspector
//! SPA (TASK `inspector-spa` §3, FROZEN @ v1). The asset is embedded in the
//! binary via `include_str!` (no runtime file dependency), so the published
//! `lunaris-server` binary is self-contained.
//!
//! The shell itself carries no secret — the recall Bearer token is entered by
//! the user at runtime and persisted to `localStorage`; every data-bearing
//! `/v1/*` call the shell makes is still `scoped_auth("recall")`-gated
//! server-side. A restrictive CSP is attached: a single-file inline-script SPA
//! requires `'unsafe-inline'` for script/style, but `connect-src 'self'` pins
//! fetches to the same origin and `textContent`-only rendering (in the shell)
//! closes the XSS vector that `'unsafe-inline'` would otherwise widen.

use axum::http::header;
use axum::response::{Html, IntoResponse};

/// The embedded single-file SPA.
const INSPECTOR_HTML: &str = include_str!("../../static/inspector.html");

/// Content-Security-Policy for the shell. `connect-src 'self'` confines all
/// `fetch` to the serving origin (no exfiltration); `'unsafe-inline'` is
/// required for the inline `<script>`/`<style>` of a single-file SPA.
const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; \
     style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; \
     base-uri 'none'; form-action 'none'";

/// Handler for `GET /` — serves the read-only Inspector shell.
pub async fn ui_handler() -> impl IntoResponse {
    ([(header::CONTENT_SECURITY_POLICY, CSP)], Html(INSPECTOR_HTML))
}
