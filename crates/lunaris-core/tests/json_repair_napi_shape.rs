//! Defect F20 — the TypeScript SDK cannot construct an `Hlc`.
//!
//! Every generated napi binding lowers its typed parameters through
//! `serde_json::from_value::<T>()`, and napi-rs hands JavaScript numbers over
//! as `serde_json::Number`s that have already lost their integer-ness above
//! `u32::MAX`. So `.between()` and `.as_of()` — the whole bi-temporal
//! time-travel surface — reject every real millisecond timestamp a TypeScript
//! caller can pass.
//!
//! These tests pin three separate things, and they are separate on purpose:
//!
//! 1. **The cause** — [`napi_number`] replicates napi 3.8.5's
//!    `impl FromNapiValue for serde_json::Number` byte-for-byte. If a napi
//!    upgrade fixes the upstream branch, the test that asserts a millisecond
//!    timestamp arrives as a float is the one that goes red, and it names the
//!    file to re-read.
//! 2. **The symptom** — plain `serde_json::from_value` really does reject
//!    that shape. If serde ever starts coercing integral floats, this goes red
//!    and the repair below becomes dead weight worth deleting.
//! 3. **The fix** — `restore_integral_numbers` makes the shape deserialise,
//!    without inventing precision and without quietly rescuing values that
//!    should still be rejected.

use lunaris_core::hlc::Hlc;
use lunaris_core::json_repair::restore_integral_numbers;
use serde::Deserialize;
use serde_json::{Number, Value, json};

/// Byte-for-byte replica of napi 3.8.5
/// `src/bindgen_runtime/js_values/serde.rs::<Number as FromNapiValue>`.
/// A JavaScript number is always an f64 on the wire; this is the only thing
/// that decides whether Rust sees an integer or a float.
fn napi_number(js: f64) -> Value {
    let n = if js.trunc() == js {
        if js >= 0.0f64 && js <= u32::MAX as f64 {
            Some(Number::from(js as u32))
        } else if js < 0.0f64 && js >= i32::MIN as f64 {
            Some(Number::from(js as i32))
        } else {
            Number::from_f64(js)
        }
    } else {
        Number::from_f64(js)
    };
    Value::Number(n.expect("napi would have raised InvalidArg"))
}

/// The `{ wall_ms, counter, node_id }` object exactly as it reaches the
/// generated binding when JavaScript passes an ordinary object literal.
fn napi_hlc(wall_ms: f64, counter: f64, node_id: f64) -> Value {
    json!({
        "wall_ms": napi_number(wall_ms),
        "counter": napi_number(counter),
        "node_id": napi_number(node_id),
    })
}

/// The asymmetry that let F20 survive: the two small fields keep their
/// integer-ness and the one large field does not. A test written with small
/// numbers passes and proves nothing.
#[test]
fn napi_keeps_small_integers_but_floats_every_millisecond_timestamp() {
    let hlc = napi_hlc(1_736_467_200_000.0, 0.0, 0.0);

    assert!(
        hlc["counter"].is_u64(),
        "napi is expected to preserve integer-ness below u32::MAX; got {:?}",
        hlc["counter"]
    );
    assert!(
        hlc["node_id"].is_u64(),
        "napi is expected to preserve integer-ness below u32::MAX; got {:?}",
        hlc["node_id"]
    );
    assert!(
        hlc["wall_ms"].is_f64(),
        "F20's cause has changed: napi no longer floats integers above u32::MAX. \
         Re-read napi's `impl FromNapiValue for serde_json::Number` and, if it now \
         preserves integer-ness, delete lunaris_core::json_repair and its call sites. \
         Got {:?}",
        hlc["wall_ms"]
    );

    // The boundary itself, stated so the range is not folklore: u32::MAX is
    // the last integer napi preserves, and the next one is not.
    assert!(napi_number(u32::MAX as f64).is_u64());
    assert!(napi_number(u32::MAX as f64 + 1.0).is_f64());
}

/// The symptom, at the exact call shape the generated bindings use.
#[test]
fn plain_from_value_rejects_the_napi_hlc_shape() {
    let err = serde_json::from_value::<Hlc>(napi_hlc(1_736_467_200_000.0, 0.0, 0.0))
        .expect_err("serde is expected to reject a float for a u64 field");
    let msg = err.to_string();
    assert!(
        msg.contains("floating point"),
        "expected a float-vs-u64 type error, got: {msg}"
    );
}

/// The fix, asserted on the value and not merely on the absence of an error.
#[test]
fn restore_integral_numbers_makes_the_napi_hlc_shape_deserialize() {
    let repaired = restore_integral_numbers(napi_hlc(1_736_467_200_000.0, 7.0, 3.0));
    let hlc: Hlc = serde_json::from_value(repaired).expect("repaired shape must deserialize");
    assert_eq!(hlc.wall_ms, 1_736_467_200_000);
    assert_eq!(hlc.counter, 7);
    assert_eq!(hlc.node_id, 3);
}

/// `CodeRepoMemory.ingestCommit(sha, committerDateUnixMs, chunks)` lowers into
/// an `i64`, so F20 is not an `Hlc`-only defect — anything millisecond-shaped
/// is affected, in both signs.
#[derive(Debug, Deserialize, PartialEq)]
struct SignedMillis {
    at: i64,
}

#[test]
fn negative_millisecond_timestamps_are_repaired_too() {
    let before_epoch = -1_736_467_200_000.0;
    let wire = json!({ "at": napi_number(before_epoch) });
    assert!(wire["at"].is_f64(), "napi should have floated this too");

    serde_json::from_value::<SignedMillis>(wire.clone())
        .expect_err("the unrepaired shape must still be rejected");

    let got: SignedMillis =
        serde_json::from_value(restore_integral_numbers(wire)).expect("repair must fix it");
    assert_eq!(got, SignedMillis { at: -1_736_467_200_000 });
}

#[test]
fn genuine_fractional_values_are_left_alone() {
    let repaired = restore_integral_numbers(json!({ "score": 0.375, "ratio": -2.5 }));
    assert_eq!(repaired["score"].as_f64(), Some(0.375));
    assert_eq!(repaired["ratio"].as_f64(), Some(-2.5));
    assert!(repaired["score"].is_f64() && repaired["ratio"].is_f64());
}

/// An integral float that a caller meant as a float still deserialises into an
/// `f64` field after repair — serde reads `1` into an `f64` happily. This is
/// the regression the repair could plausibly have caused, so it is pinned.
#[derive(Debug, Deserialize)]
struct FloatField {
    weight: f64,
}

#[test]
fn an_integral_float_still_deserializes_into_a_float_field() {
    let got: FloatField =
        serde_json::from_value(restore_integral_numbers(json!({ "weight": 1.0 })))
            .expect("an f64 field must accept a repaired integer");
    assert_eq!(got.weight, 1.0);
}

#[test]
fn repair_reaches_every_depth_and_every_position() {
    let repaired = restore_integral_numbers(json!({
        "outer": {
            "list": [napi_number(5_000_000_000.0), {"deep": napi_number(9_000_000_000.0)}],
        },
        "top": napi_number(6_000_000_000.0),
    }));

    assert_eq!(repaired["top"].as_u64(), Some(6_000_000_000));
    assert_eq!(repaired["outer"]["list"][0].as_u64(), Some(5_000_000_000));
    assert_eq!(repaired["outer"]["list"][1]["deep"].as_u64(), Some(9_000_000_000));
}

#[test]
fn non_numeric_leaves_and_structure_survive_untouched() {
    let original = json!({
        "s": "1736467200000",
        "b": true,
        "n": Value::Null,
        "empty_obj": {},
        "empty_arr": [],
        "arr": ["a", null, false],
    });
    assert_eq!(restore_integral_numbers(original.clone()), original);
}

/// Magnitudes outside `i64`/`u64` must stay floats so the typed
/// deserialisation still rejects them loudly. Silently wrapping a value the
/// caller never wrote would be worse than the defect being fixed.
#[test]
fn out_of_range_magnitudes_are_not_coerced() {
    let too_big = Value::Number(Number::from_f64(1e30).unwrap());
    let too_small = Value::Number(Number::from_f64(-1e30).unwrap());
    assert!(restore_integral_numbers(too_big).is_f64());
    assert!(restore_integral_numbers(too_small).is_f64());

    // `u64::MAX as f64` rounds UP to 2^64, which is not a u64 at all; coercing
    // it would saturate to u64::MAX and hand back a number nobody wrote.
    let past_u64 = Value::Number(Number::from_f64(u64::MAX as f64).unwrap());
    assert!(restore_integral_numbers(past_u64).is_f64());
}

#[test]
fn numbers_that_already_carry_integerness_are_returned_unchanged() {
    let original = json!({ "u": 1_736_467_200_000u64, "i": -42i64, "zero": 0 });
    assert_eq!(restore_integral_numbers(original.clone()), original);
}
