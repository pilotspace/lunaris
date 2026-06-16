//! Pure scoring primitives for the eval gauntlet (EVAL-01..03).
//!
//! NO env / network / backend access — these are unit-driven, and the live
//! harness arms (`longmemeval` / `locomo` / `er_f1`) call them after collecting
//! recall hits. Keeping the math pure is what lets the gate be tested without a
//! live Moon or model; the numbers themselves populate at HUMAN-UAT on a
//! weights-cached runner.

#![forbid(unsafe_code)]

/// One query's recall outcome: the top-k recalled texts plus the gold answer
/// the recall pipeline was supposed to surface.
#[derive(Debug, Clone)]
pub struct QueryHit {
    pub topk_texts: Vec<String>,
    pub gold_answer: String,
}

/// Normalize text for tolerant substring matching: lowercase + trim + collapse
/// internal whitespace runs to a single space.
pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Micro-averaged set F1 over `(entity, type)` pairs.
///
/// `tp = |pred ∩ gold|`, `P = tp/|pred|`, `R = tp/|gold|`, `F1 = 2PR/(P+R)`.
/// Both empty → `1.0` (vacuously perfect); exactly one side empty → `0.0`;
/// never `NaN`.
pub fn compute_f1(predicted: &[(String, String)], gold: &[(String, String)]) -> f64 {
    use std::collections::HashSet;
    // Exact-match over NORMALIZED `(entity, type)` — case/whitespace folded so
    // the extractor's surface form lines up with the gold span (contract §3).
    let normset = |pairs: &[(String, String)]| -> HashSet<(String, String)> {
        pairs.iter().map(|(e, t)| (normalize(e), normalize(t))).collect()
    };
    let pred = normset(predicted);
    let gold = normset(gold);
    match (pred.is_empty(), gold.is_empty()) {
        (true, true) => return 1.0, // both empty → vacuously perfect
        (true, false) | (false, true) => return 0.0, // exactly one side empty
        (false, false) => {}
    }
    let tp = pred.intersection(&gold).count() as f64;
    if tp == 0.0 {
        return 0.0; // disjoint sets → F1 0, never NaN from a 0/0 below
    }
    let precision = tp / pred.len() as f64;
    let recall = tp / gold.len() as f64;
    2.0 * precision * recall / (precision + recall)
}

/// Recall-based J-score (the autonomously-computable proxy; the LLM-judge
/// variant is a HUMAN-UAT refinement): `100 * (#queries whose top-k contains
/// the normalized gold answer) / N`. Empty → `0.0`.
pub fn recall_j_score(per_query: &[QueryHit]) -> f64 {
    if per_query.is_empty() {
        return 0.0; // no queries → 0.0, never NaN from a 0/0
    }
    let hits = per_query
        .iter()
        .filter(|q| {
            let gold = normalize(&q.gold_answer);
            q.topk_texts.iter().any(|t| normalize(t).contains(&gold))
        })
        .count();
    100.0 * hits as f64 / per_query.len() as f64
}
