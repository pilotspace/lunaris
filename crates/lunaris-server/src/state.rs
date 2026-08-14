//! Plan 05-01 — shared `AppState` plumbing for axum handlers.
//!
//! All three fields are `Arc`-shared so cloning across handlers + middleware is
//! cheap (matches the `lunaris::Lunaris` Arc-of-trait shape from
//! `crates/lunaris/src/handle.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use lunaris::Lunaris;

/// Per-tenant bearer-token claims loaded from `--tokens-file`.
///
/// The on-disk JSON shape mirrors CONTEXT.md D-07 verbatim:
/// ```json
/// { "<token>": { "tenant": "<id>", "scopes": ["ingest", "recall", "forget"] } }
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TokenClaims {
    pub tenant: String,
    pub scopes: Vec<String>,
}

/// In-memory token map: bearer token string → claims.
pub type TokenMap = HashMap<String, TokenClaims>;

/// Three-surface toggle (PATTERNS.md Shared Pattern 4) — runtime knobs the
/// operator can flip without restart. v0 wires two flags; Plan 05-05 will
/// extend this with metrics + tracing toggles.
#[derive(Debug, Default)]
pub struct RuntimeFlags {
    pub metrics_disabled: RwLock<bool>,
    pub rate_limit_enabled: RwLock<bool>,
}

/// Shared application state. Cloned per request via `axum::extract::State`.
#[derive(Clone)]
pub struct AppState {
    pub lunaris: Arc<Lunaris>,
    pub tokens: Arc<TokenMap>,
    pub runtime_flags: Arc<RuntimeFlags>,
    /// 0.6.2 P0-3 — rate-limited `/readyz` prober. Process-wide so the write
    /// canary fires at most once per `readiness::CACHE_TTL` no matter how many
    /// probes arrive.
    pub readiness: Arc<crate::readiness::Readiness>,
}

impl AppState {
    /// Construct a fresh `AppState` with the given lunaris handle + tokens map.
    /// Rate-limit defaults to enabled; metrics defaults to enabled.
    pub fn new(lunaris: Arc<Lunaris>, tokens: TokenMap) -> Self {
        let flags = RuntimeFlags::default();
        *flags.rate_limit_enabled.write() = true;
        Self {
            lunaris,
            tokens: Arc::new(tokens),
            runtime_flags: Arc::new(flags),
            readiness: Arc::new(crate::readiness::Readiness::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_claims_deserialize_matches_d07_shape() {
        let raw = serde_json::json!({
            "tok-abc": { "tenant": "t-1", "scopes": ["ingest", "recall"] }
        });
        let map: TokenMap = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(map.len(), 1);
        let c = map.get("tok-abc").expect("entry");
        assert_eq!(c.tenant, "t-1");
        assert_eq!(c.scopes, vec!["ingest", "recall"]);
    }

    #[test]
    fn runtime_flags_default_to_off() {
        let f = RuntimeFlags::default();
        assert!(!*f.metrics_disabled.read());
        assert!(!*f.rate_limit_enabled.read());
    }
}
