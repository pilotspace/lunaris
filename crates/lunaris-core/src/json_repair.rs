//! Repair JSON numbers that lost their integer-ness in transit.
//!
//! # Why this exists (defect F20)
//!
//! napi-rs converts a JavaScript number into a [`serde_json::Number`] in
//! `impl FromNapiValue for Number` (napi 3.8.5,
//! `src/bindgen_runtime/js_values/serde.rs`). That conversion only preserves
//! integer-ness for values that fit in `u32` or `i32`:
//!
//! ```text
//! let n = if n.trunc() == n {
//!     if n >= 0.0 && n <= u32::MAX as f64 { Some(Number::from(n as u32)) }
//!     else if n < 0.0 && n >= i32::MIN as f64 { Some(Number::from(n as i32)) }
//!     else { Number::from_f64(n) }          // <-- integral, but now a FLOAT
//! } else { Number::from_f64(n) };
//! ```
//!
//! Every integer above `u32::MAX` (4_294_967_295) therefore arrives on the
//! Rust side as a float. That is not an exotic range: it is **every
//! millisecond timestamp since 1970-02-19**. So an ordinary JavaScript
//! `{ wall_ms: 1736467200000, counter: 0, node_id: 0 }` deserialises as
//! `1736467200000.0` and `serde_json::from_value::<Hlc>` rejects it with
//! `invalid type: floating point 1736467200000.0, expected u64` — while
//! `counter` and `node_id`, being small, convert fine. That asymmetry is
//! why the defect survived: a test written with small numbers passes.
//!
//! [`restore_integral_numbers`] walks the value tree and turns every
//! integral float back into an integer `Number` before the typed
//! deserialisation runs, so the u64/i64/u32 fields of a domain type accept
//! what JavaScript actually meant.
//!
//! # Precision
//!
//! This does not invent precision. A JavaScript number is an f64, so an
//! integer above 2^53 was already inexact before it reached Rust; this
//! function preserves whatever f64 held rather than adding error. Values
//! outside the `i64`/`u64` range, and non-finite values, are left as
//! floats so the typed deserialisation still rejects them loudly instead of
//! wrapping around.

use serde_json::Value;

/// Recursively rewrite every integral floating-point number in `value` as an
/// integer number, leaving genuine fractional values, non-finite values, and
/// out-of-range magnitudes untouched.
///
/// Structure (objects, arrays, key order) and every non-numeric leaf are
/// preserved exactly.
pub fn restore_integral_numbers(value: Value) -> Value {
    // RED stub — today's behaviour: no repair at all.
    value
}
