//! Red-first unit tests for the pure eval scorers (frozen contract §3,
//! eval-gauntlet-ci-gate). These drive `compute_f1` + `recall_j_score`
//! directly — no env, no backend — so the gate is provable without live
//! models. RED until the BUILD phase fills the `unimplemented!()` bodies.

use lunaris_bench::eval::score::{QueryHit, compute_f1, recall_j_score};

fn pair(e: &str, t: &str) -> (String, String) {
    (e.to_string(), t.to_string())
}

#[test]
fn compute_f1_known_set() {
    // pred = 3 pairs, gold = 3 pairs, overlap = 2 → P = R = 2/3 → F1 = 2/3.
    let pred = vec![pair("Alice", "person"), pair("Acme", "corporation"), pair("Foo", "product")];
    let gold = vec![pair("Alice", "person"), pair("Acme", "corporation"), pair("NYC", "location")];
    let f1 = compute_f1(&pred, &gold);
    assert!((f1 - 2.0 / 3.0).abs() < 1e-6, "F1 = {f1}, expected 0.6667");
}

#[test]
fn compute_f1_perfect_and_empty() {
    let g = vec![pair("Alice", "person")];
    assert_eq!(compute_f1(&g, &g), 1.0, "identical sets → 1.0");
    assert_eq!(compute_f1(&[], &[]), 1.0, "both empty → vacuously 1.0");
    assert_eq!(compute_f1(&g, &[]), 0.0, "pred-only → 0.0");
    assert_eq!(compute_f1(&[], &g), 0.0, "gold-only → 0.0");
}

#[test]
fn recall_j_score_counts_topk_gold_hits() {
    let hit = |gold: &str| QueryHit {
        topk_texts: vec![format!("... the {gold} ...")],
        gold_answer: gold.into(),
    };
    let miss = |gold: &str| QueryHit {
        topk_texts: vec!["unrelated text".into()],
        gold_answer: gold.into(),
    };
    let per = vec![hit("paris"), hit("london"), hit("rome"), miss("tokyo")];
    assert!((recall_j_score(&per) - 75.0).abs() < 1e-9, "3 of 4 hits → 75.0");
    assert_eq!(recall_j_score(&[]), 0.0, "empty → 0.0, not NaN");
}

#[test]
fn recall_j_score_normalizes_case_and_whitespace() {
    let per = vec![QueryHit {
        topk_texts: vec!["The   PARIS   museum".into()],
        gold_answer: "paris".into(),
    }];
    assert_eq!(recall_j_score(&per), 100.0, "case+whitespace-insensitive match");
}
