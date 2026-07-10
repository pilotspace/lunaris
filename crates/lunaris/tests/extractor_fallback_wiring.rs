//! extractor-fallback-wiring (Half A) — structural wiring guard.
//!
//! The production `remote_extractor_from_env()` Ok arm must wrap the real
//! extractor via `fallback_wrap` (not return the bare `Arc::new(e)`). That
//! arm needs a live cloud provider to run, so it cannot be exercised in CI;
//! this guard pins the call-site instead. The behavioral contract of the wrap
//! itself (transient -> Noop, terminal -> propagate) is covered by the
//! `fallback_wrap_*` tests in lunaris-extract. This test reads handle.rs as a
//! string (it is a separate file, so there is no self-match risk).
//!
//! llama.cpp-only cutover (Phase C): the original guard pinned the candle
//! cache-hit arm (`fallback_wrap(e, "gemma-3-4b-it")`); that arm was deleted
//! with the local extractor — the remote cloud-api arm is now the only
//! production path that constructs a real extractor.

const HANDLE_SRC: &str = include_str!("../src/handle.rs");

#[test]
fn remote_extractor_ok_arm_wraps_in_fallback() {
    assert!(
        HANDLE_SRC.contains("fallback_wrap(e, &label)"),
        "remote_extractor_from_env's Ok arm must wrap the real extractor via \
         fallback_wrap so FallbackExtractor+CircuitBreaker are on the production path"
    );
    assert!(
        !HANDLE_SRC.contains("Ok(e) => Arc::new(e) as Arc<dyn Extractor>"),
        "the remote Ok arm must not return the bare Arc::new(e); it must go \
         through fallback_wrap"
    );
}
