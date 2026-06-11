//! ADD task `sq8-quantization-optin` — URL-grammar + Quantization parse suite
//! (contract FROZEN @ v1, 2026-06-11). No live backend needed: the invalid
//! value must be rejected BEFORE any network IO.

use std::str::FromStr;

use lunaris_core::error::StorageError;
use lunaris_storage_moon::client::Quantization;

#[test]
fn from_str_accepts_all_tiers_case_insensitively() {
    for (input, expect) in [
        ("sq8", Quantization::Sq8),
        ("SQ8", Quantization::Sq8),
        ("Sq8", Quantization::Sq8),
        ("tq1", Quantization::Tq1),
        ("TQ2", Quantization::Tq2),
        ("tq3", Quantization::Tq3),
        ("tq4", Quantization::Tq4),
    ] {
        assert_eq!(Quantization::from_str(input).expect("valid tier"), expect, "input {input}");
    }
}

#[test]
fn from_str_rejects_unknown_values_with_named_code() {
    for bad in ["foo", "", "sq16", "tq5", "8"] {
        let err = Quantization::from_str(bad).expect_err("invalid tier must be rejected");
        assert!(
            err.to_string().contains("moon_invalid_quantization"),
            "error must carry the named code for {bad:?}, got: {err}"
        );
    }
}

#[test]
fn wire_rendering_is_uppercase() {
    assert_eq!(Quantization::Sq8.as_wire(), "SQ8");
    assert_eq!(Quantization::Tq1.as_wire(), "TQ1");
    assert_eq!(Quantization::Tq4.as_wire(), "TQ4");
}

/// §2 Reject — `?quant=foo` must fail BEFORE any connect attempt. Port 1 is
/// intentionally dead: a connection attempt would surface a transport error
/// ("connection refused" / timeout), NOT the named parse code.
#[tokio::test]
async fn invalid_quant_rejected_before_io() {
    let err = lunaris_storage_moon::MoonStorage::connect("moon://127.0.0.1:1?quant=foo")
        .await
        .expect_err("invalid quant must be rejected");
    match err {
        StorageError::Backend(msg) => {
            assert!(
                msg.contains("moon_invalid_quantization"),
                "must be the parse error, not a transport error; got: {msg}"
            );
        }
        other => panic!("expected Backend(moon_invalid_quantization…), got {other:?}"),
    }
}
