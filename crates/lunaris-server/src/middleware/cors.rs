//! Plan 05-01 — CORS layer factory (D-09).
//!
//! `--cors-origins *` → `CorsLayer::permissive()` (dev mode).
//! `--cors-origins https://a.com,https://b.com` → allow-listed.

use axum::http::HeaderValue;
use tower_http::cors::{Any, CorsLayer};

/// Build a CORS layer from the `--cors-origins` CSV spec.
///
/// `*` (the default) returns a permissive layer for dev. Any other value is
/// parsed as a comma-separated allow-list; entries that fail header-value
/// parsing are silently dropped (operator should pass valid origins).
pub fn cors_layer(spec: &str) -> CorsLayer {
    if spec.trim() == "*" {
        return CorsLayer::permissive();
    }
    let mut layer = CorsLayer::new().allow_methods(Any).allow_headers(Any);
    for origin in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(value) = origin.parse::<HeaderValue>() {
            layer = layer.allow_origin(value);
        }
    }
    layer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissive_when_star() {
        // Smoke test — just verify the function returns without panicking.
        let _ = cors_layer("*");
    }

    #[test]
    fn allow_list_parses_csv() {
        let _ = cors_layer("https://a.com,https://b.com");
    }

    #[test]
    fn empty_segments_skipped() {
        let _ = cors_layer("https://a.com,,https://b.com,");
    }
}
