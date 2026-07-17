//! ADD task activation-ledger — RED suite for `lunaris_core::activation` +
//! the `activation_key` keyspace helper (§4 test_plan, lunaris-core rows).
//!
//! Pure-data tests: no storage, no async. `ActivationRecord::apply` /
//! `::activation` are exercised directly per the frozen §3 CONTRACT.

use lunaris_core::activation::{
    ActivationRecord, BOOST_CAP, Grain, RefSignal, Strength, WEIGHT_STRONG, WEIGHT_WEAK,
    boost_prior,
};
use lunaris_core::keyspace::activation_key;
use lunaris_core::Scope;
use ulid::Ulid;

/// Scenario 1 — "reference signal upserts one summary record": a weak
/// turn-grain signal seeds `n=1, weighted=1.0, grain=turn, strength=weak`
/// with both walls set to the signal's wall time; a second strong
/// tool_call-grain signal updates the SAME record in place
/// (`n=2, weighted=WEIGHT_WEAK+WEIGHT_STRONG=4.0`, `last_ref_wall` advances,
/// `first_ref_wall` stays put).
#[test]
fn upsert_math_first_and_second_signal() {
    let id = Ulid::new();
    let mut record = ActivationRecord::default();

    let first = RefSignal { id, grain: Grain::Turn, strength: Strength::Weak };
    record.apply(&first, 1_000);
    assert_eq!(record.n, 1);
    assert_eq!(record.weighted, WEIGHT_WEAK);
    assert_eq!(record.first_ref_wall, 1_000);
    assert_eq!(record.last_ref_wall, 1_000);
    assert_eq!(record.last_grain, Grain::Turn);
    assert_eq!(record.last_strength, Strength::Weak);
    assert_eq!(record.v, 1);

    let second = RefSignal { id, grain: Grain::ToolCall, strength: Strength::Strong };
    record.apply(&second, 2_000);
    assert_eq!(record.n, 2, "n increments on the second signal");
    assert_eq!(
        record.weighted,
        WEIGHT_WEAK + WEIGHT_STRONG,
        "weighted running sum accumulates weak(1.0) + strong(3.0) = 4.0"
    );
    assert_eq!(record.first_ref_wall, 1_000, "first_ref_wall must NOT change on later signals");
    assert_eq!(record.last_ref_wall, 2_000, "last_ref_wall must advance");
    assert_eq!(record.last_grain, Grain::ToolCall, "last_grain reflects the newest signal");
    assert_eq!(record.last_strength, Strength::Strong, "last_strength reflects the newest signal");
}

/// Scenario 4 — "activation decays with wall age": two identical-count
/// records (one strong ref each), one referenced long ago, one referenced
/// recently relative to `now`. The recomputed activation of the OLD record
/// must be strictly lower than the RECENT record's. Separately: `boost_prior`
/// never exceeds `BOOST_CAP` regardless of how large the weighted sum grows
/// (cap-safety pinned independent of the decay/age assertion above).
#[test]
fn activation_decays_with_age_and_is_capped() {
    let id = Ulid::new();
    let decay = 0.5;
    let now = 1_000_000u64;

    let mut old = ActivationRecord::default();
    old.apply(&RefSignal { id, grain: Grain::Turn, strength: Strength::Strong }, 0);

    let mut recent = ActivationRecord::default();
    recent.apply(&RefSignal { id, grain: Grain::Turn, strength: Strength::Strong }, now - 5);

    let old_activation = old.activation(now, decay);
    let recent_activation = recent.activation(now, decay);
    assert!(
        recent_activation > old_activation,
        "recently-referenced record must score higher: recent={recent_activation}, old={old_activation}"
    );

    // Cap-safety: an enormous weighted sum (heavy, recent reference traffic)
    // must never push the derived boost above BOOST_CAP.
    let mut heavy = ActivationRecord::default();
    for i in 0..100_000u64 {
        heavy.apply(&RefSignal { id, grain: Grain::Turn, strength: Strength::Strong }, i);
    }
    let heavy_activation = heavy.activation(now, decay);
    let heavy_boost = boost_prior(heavy_activation);
    assert!(
        heavy_boost <= BOOST_CAP,
        "boost must never exceed BOOST_CAP regardless of ref count; got {heavy_boost}"
    );
    assert!(heavy_boost >= 0.0, "boost must never be negative");
}

/// Reject 3 — "unknown grain/strength rejected at the type level": a
/// `RefSignal` wire payload naming an unrecognized `grain` or `strength`
/// string MUST fail to deserialize (enum, not a free string) rather than
/// silently coercing to a default variant.
#[test]
fn serde_rejects_unknown_grain_and_strength() {
    let bad_grain = r#"{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","grain":"week","strength":"weak"}"#;
    assert!(
        serde_json::from_str::<RefSignal>(bad_grain).is_err(),
        "grain=\"week\" must be rejected — Grain is a closed enum"
    );

    let bad_strength = r#"{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","grain":"turn","strength":"medium"}"#;
    assert!(
        serde_json::from_str::<RefSignal>(bad_strength).is_err(),
        "strength=\"medium\" must be rejected — Strength is a closed enum"
    );

    // Sanity: a well-formed payload decodes fine (proves the two rejections
    // above are about the enum values, not the shape).
    let good = r#"{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","grain":"tool_call","strength":"strong"}"#;
    let decoded: RefSignal = serde_json::from_str(good).expect("well-formed payload must decode");
    assert_eq!(decoded.grain, Grain::ToolCall);
    assert_eq!(decoded.strength, Strength::Strong);
}

/// The canonical KV key mint: `lunaris:{scope}:activation:{ulid}`, matching
/// every other `*_key` helper in `lunaris_core::keyspace` byte-for-byte.
#[test]
fn activation_key_format_is_scoped() {
    let scope = Scope::new("acme.agent-1").unwrap();
    let id = Ulid::from_string("01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
    assert_eq!(
        activation_key(&scope, id),
        b"lunaris:acme.agent-1:activation:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
    );

    // Distinct scopes never alias (mirrors `scoped_keys_differ_across_scopes`).
    let other = Scope::new("acme.agent-2").unwrap();
    assert_ne!(activation_key(&scope, id), activation_key(&other, id));
}
