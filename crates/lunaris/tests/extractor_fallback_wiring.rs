//! extractor-fallback-wiring (Half A) — structural wiring guard.
//!
//! The production `default_extractor()` candle cache-hit arm must wrap the real
//! extractor via `fallback_wrap` (not return the bare `Arc::new(e)`). That arm
//! needs ~3GB gemma-3-4b-it weights to run, so it cannot be exercised in CI;
//! this guard pins the call-site instead. The behavioral contract of the wrap
//! itself (transient -> Noop, terminal -> propagate) is covered by the
//! `fallback_wrap_*` tests in lunaris-extract. This test reads handle.rs as a
//! string (it is a separate file, so there is no self-match risk).

const HANDLE_SRC: &str = include_str!("../src/handle.rs");

#[test]
fn default_extractor_wraps_candle_primary_in_fallback() {
    assert!(
        HANDLE_SRC.contains("fallback_wrap(e, \"gemma-3-4b-it\")"),
        "default_extractor's candle cache-hit arm must wrap the real extractor via \
         fallback_wrap(e, \"gemma-3-4b-it\")"
    );
    assert!(
        !HANDLE_SRC.contains("Ok(e) => Arc::new(e) as Arc<dyn Extractor>"),
        "the candle OK arm must no longer return the bare Arc::new(e); it must go \
         through fallback_wrap so FallbackExtractor+CircuitBreaker are on the production path"
    );
}
