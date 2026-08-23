//! F14 — the SDK plan parser: a JSON operator tree becomes a real retriever.
//!
//! The Python and TypeScript DSLs advertise composition (`.and_()`,
//! `.fuse_rrf()`, `.top()`) but their FFI carried ONE leg: a flat
//! `{index, k}` plan. Everything else was collapsed away, and since a
//! silently-collapsed plan answers a different question than the one written,
//! the SDKs refused those plans outright. That refusal was correct and is not
//! what these tests replace — what they replace is the *reason* for it.
//!
//! `retriever_from_json` is the one parser both SDKs marshal into, so the
//! shape a caller writes in Python is the shape the engine runs. Assertions
//! are on [`lunaris_retrieve::composition::plan_repr`], the canonical
//! rendering of a built tree: it names every operator and its parameters, so
//! an assertion on it fails when a leg is dropped, reordered, or re-parameterized.

use lunaris_retrieve::composition::plan_repr;
use lunaris_retrieve::plan::retriever_from_json;

fn repr_of(json: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(json).expect("test JSON parses");
    let r = retriever_from_json(&v).expect("plan builds");
    plan_repr(r.as_ref())
}

/// The exact plan `test_dsl_parity.py::test_vector_and_keyword_fuse_top_shape`
/// writes. Before F14 this could only be refused; it must now run as written.
#[test]
fn the_two_leg_fused_plan_the_sdk_parity_tests_write_builds_both_legs() {
    let got = repr_of(
        r#"{"op":"top","n":5,"child":
             {"op":"fuse_rrf","k":60,"child":
               {"op":"and",
                "left":{"op":"vector","index":"chunks","k":30},
                "right":{"op":"keyword","index":"chunks","k":30}}}}"#,
    );
    assert_eq!(
        got, "top(n=5,fuse_rrf(k=60,and(vector(chunks,k=30),bm25(chunks,k=30))))",
        "the built tree must carry BOTH legs, the fuse and the top"
    );
}

/// Operand order is caller-visible: the old collapse let the last leg visited
/// win `index` and `k`, so flipping the operands changed which index was
/// searched with no diagnostic. Now the two orders build two different trees.
#[test]
fn flipping_the_operands_builds_a_different_tree_not_the_same_one() {
    let a = repr_of(
        r#"{"op":"and","left":{"op":"vector","index":"chunks","k":10},
                       "right":{"op":"keyword","index":"facts","k":20}}"#,
    );
    let b = repr_of(
        r#"{"op":"and","left":{"op":"keyword","index":"facts","k":20},
                       "right":{"op":"vector","index":"chunks","k":10}}"#,
    );
    assert_eq!(a, "and(vector(chunks,k=10),bm25(facts,k=20))");
    assert_eq!(b, "and(bm25(facts,k=20),vector(chunks,k=10))");
    assert_ne!(a, b, "the two operand orders must not collapse to one tree");
}

/// A graph leg used to vanish entirely. Its seeds and hop count must survive
/// into the built tree, which means `plan_repr` has to render `Graph` — an
/// operator it previously fell through to `<opaque>` on, where every graph
/// plan looks identical to every other.
#[test]
fn a_graph_leg_carries_its_seeds_and_hops() {
    let got = repr_of(r#"{"op":"graph","seeds":[{"name":"Alice","type":"Person"}],"hops":2}"#);
    assert_eq!(got, "graph(seeds=1,hops=2)");

    let deeper = repr_of(
        r#"{"op":"graph","seeds":[{"name":"Alice","type":"Person"},
                                  {"name":"Bob","type":"Person"}],"hops":3}"#,
    );
    assert_eq!(deeper, "graph(seeds=2,hops=3)");
    assert_ne!(got, deeper, "seed count and hops must be visible in the repr");
}

/// Seeds may be given as the 32-char hex `EntityId` (what the engine emits)
/// or as a `{name, type}` pair (what a human writes). Both resolve to the
/// same anchor when they name the same entity.
#[test]
fn a_hex_seed_and_the_name_type_pair_that_derives_it_are_the_same_anchor() {
    let by_name: serde_json::Value = serde_json::from_str(
        r#"{"op":"graph","seeds":[{"name":"Alice","type":"Person"}],"hops":1}"#,
    )
    .unwrap();
    let built = retriever_from_json(&by_name).expect("name/type seed builds");
    let hex = lunaris_retrieve::plan::seed_hex(built.as_ref())
        .expect("a graph root exposes its seed ids");
    assert_eq!(hex.len(), 1);
    assert_eq!(hex[0].len(), 32, "an EntityId renders as 32 hex chars");

    let by_hex: serde_json::Value =
        serde_json::from_str(&format!(r#"{{"op":"graph","seeds":["{}"],"hops":1}}"#, hex[0]))
            .unwrap();
    let built2 = retriever_from_json(&by_hex).expect("hex seed builds");
    assert_eq!(
        lunaris_retrieve::plan::seed_hex(built2.as_ref()).unwrap(),
        hex,
        "the hex form must resolve to the same anchor as the name/type pair"
    );
}

/// An op the parser does not know must be an ERROR. The whole point of F14 is
/// that the plan you write is the plan that runs; silently skipping an
/// unrecognized node reintroduces exactly the defect this replaces.
#[test]
fn an_unrecognized_op_is_an_error_not_a_silent_skip() {
    let v: serde_json::Value =
        serde_json::from_str(r#"{"op":"raptor_descend","levels":3}"#).unwrap();
    // `expect_err` would need `Box<dyn Retriever>: Debug`, which it is not.
    let msg = match retriever_from_json(&v) {
        Ok(_) => panic!("an unknown op must not build"),
        Err(e) => e.to_string(),
    };
    assert!(msg.contains("raptor_descend"), "the error must name the op it rejected: {msg}");
}

/// A missing branch is a truncated plan, not a plan with a default branch.
#[test]
fn a_missing_branch_is_an_error_not_a_defaulted_branch() {
    for json in [
        r#"{"op":"and","left":{"op":"vector","index":"chunks","k":5}}"#,
        r#"{"op":"fuse_rrf","k":60}"#,
        r#"{"op":"top","n":5}"#,
    ] {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(
            retriever_from_json(&v).is_err(),
            "a node missing its child must be rejected, not silently defaulted: {json}"
        );
    }
}

/// Vacuity floor. Every assertion above compares a `plan_repr` string, so the
/// suite is only meaningful if `plan_repr` actually distinguishes the shapes
/// it is asked about. Feed it a one-leg tree and a two-leg tree and require
/// they differ — if `plan_repr` ever regressed to `<opaque>` for these
/// operators the tests above would still pass by matching each other.
#[test]
fn the_repr_distinguishes_the_shapes_these_tests_rely_on() {
    let one = repr_of(r#"{"op":"vector","index":"chunks","k":30}"#);
    let two = repr_of(
        r#"{"op":"and","left":{"op":"vector","index":"chunks","k":30},
                       "right":{"op":"keyword","index":"chunks","k":30}}"#,
    );
    assert_ne!(one, two, "plan_repr must distinguish one leg from two");
    assert!(!one.contains("<opaque>"), "plan_repr rendered an opaque leg: {one}");
    assert!(!two.contains("<opaque>"), "plan_repr rendered an opaque leg: {two}");
}
