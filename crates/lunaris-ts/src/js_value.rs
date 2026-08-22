//! The single deserialisation seam for every generated napi binding.
//!
//! # Why this is not `serde_json::from_value` (defect F20)
//!
//! napi-rs converts a JavaScript number into a `serde_json::Number` in
//! `impl FromNapiValue for Number` (napi 3.8.5), and that conversion only
//! preserves integer-ness for values that fit in `u32` or `i32` — everything
//! larger falls through to `Number::from_f64`. The practical boundary is
//! `u32::MAX` = 4_294_967_295, which is **every millisecond timestamp since
//! 1970-02-19**.
//!
//! So an ordinary JavaScript `{ wall_ms: 1736467200000, counter: 0, node_id: 0 }`
//! reached the generated `TimelineReconstruction::between` binding as
//! `1736467200000.0` and `serde_json::from_value::<Hlc>` rejected it with
//! `invalid type: floating point 1736467200000.0, expected u64`. `.between()`
//! and `.as_of()` — the entire bi-temporal time-travel surface — were
//! unusable from TypeScript, while `counter` and `node_id`, being small,
//! converted fine. That asymmetry is why it went unnoticed.
//!
//! [`from_js`] repairs the number shape before the typed deserialisation
//! runs. The repair itself lives in `lunaris_core::json_repair` and is unit
//! tested there, because `cargo test -p lunaris-ts --lib` cannot link (napi's
//! `_napi_delete_reference` is supplied by the Node host, not the Rust test
//! harness) — a test inside this crate could never run. The end-to-end proof
//! that the generated bindings actually route through here is
//! `crates/lunaris-ts/__test__/hlc_bitemporal.spec.mts`, which crosses the
//! real FFI boundary.
//!
//! The Python SDK needs no equivalent: `pythonize` preserves integer-ness.

use serde::de::DeserializeOwned;

/// Deserialise a value that arrived across the napi boundary into `T`,
/// repairing numbers that lost their integer-ness in transit.
///
/// Drop-in replacement for `serde_json::from_value` — same signature, same
/// error type — so the generated call sites keep their
/// `.map_err(napi_err)?` shape.
pub(crate) fn from_js<T: DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, serde_json::Error> {
    serde_json::from_value(::lunaris_core::json_repair::restore_integral_numbers(value))
}
