//! Integration test for the post-hydrate recency rescorer (P0 #3).
//!
//! Mirrors the test style of `lunaris-recipes::message_stream` — build a
//! synthetic `Vec<Hit>` with varied `valid_from` stamps, run the rescorer,
//! assert the resulting ordering. No backend / hydrate plumbing required;
//! the rescorer's contract is "input Vec<Hit>, output reordered Vec<Hit>".

use std::time::Duration;

use lunaris_core::Hlc;
use lunaris_retrieve::{ActR, Exp, Hit, RecencyConfig, SourceOp, TimeSource, rescore_recency};

fn mk_hit(id: u8, score: f32, valid_from_ms: u64) -> Hit {
    Hit {
        id: vec![id],
        score,
        text: String::new(),
        source: String::new(),
        heading_path: Vec::new(),
        valid_from: Hlc { wall_ms: valid_from_ms, counter: 0, node_id: 0 },
        valid_to: None,
        degraded: false,
        rerank_applied: false,
        source_op: SourceOp::Fused,
    }
}

fn at(ms: u64) -> Hlc {
    Hlc { wall_ms: ms, counter: 0, node_id: 0 }
}

#[test]
fn exp_rescorer_reorders_three_hits_by_age() {
    // Three hits, identical pre-score (0.9), valid_from at now, now-1day,
    // now-30day. After a 7-day-half-life Exp rescore the order MUST be
    // newest → middle → oldest, regardless of original list order.
    let now_ms: u64 = 100_000_000_000;
    let day = 24 * 3600 * 1000u64;
    let mut hits = vec![
        mk_hit(3, 0.9, now_ms - 30 * day), // oldest
        mk_hit(1, 0.9, now_ms),            // newest
        mk_hit(2, 0.9, now_ms - day),      // middle
    ];
    let cfg = RecencyConfig::default(); // Exp 7-day half-life
    rescore_recency(&mut hits, at(now_ms), &cfg);
    let ids: Vec<u8> = hits.iter().map(|h| h.id[0]).collect();
    assert_eq!(ids, vec![1, 2, 3], "expected newest→middle→oldest");
    // Scores strictly descend.
    assert!(hits[0].score > hits[1].score);
    assert!(hits[1].score > hits[2].score);
}

#[test]
fn act_r_rescorer_matches_recipe_blend() {
    // ActR blend mirrors `MessageStream::recall`: prior + ln(age^-0.5).
    // With identical priors, the newer hit must end on top.
    let now_ms: u64 = 100_000_000_000;
    let hour = 3_600_000_u64;
    let mut hits = vec![
        mk_hit(10, 0.4, now_ms - 24 * hour), // 24h old
        mk_hit(11, 0.4, now_ms - hour),      // 1h old
    ];
    let cfg = RecencyConfig::new(TimeSource::ValidFrom, Box::new(ActR::default()));
    rescore_recency(&mut hits, at(now_ms), &cfg);
    assert_eq!(hits[0].id, vec![11], "1h-old hit must rank above 24h-old");

    // Direct formula check on the top hit.
    let expected_top = 0.4_f32 + (3600.0_f32).powf(-0.5).ln();
    assert!(
        (hits[0].score - expected_top).abs() < 1e-5,
        "got {}, expected {}",
        hits[0].score,
        expected_top
    );
}

#[test]
fn custom_half_life_changes_decay_curve() {
    // Same input, two different half-lives → different orderings are NOT
    // expected (newer always wins) but the score *ratio* between newer
    // and older must differ. With a 1-hour half-life, a 1-hour-old hit
    // is halved; with a 1-day half-life, the same hit decays far less.
    let now_ms: u64 = 50_000_000_000;
    let hour = 3_600_000_u64;

    let mut hits_a = vec![mk_hit(1, 1.0, now_ms - hour)];
    let cfg_a =
        RecencyConfig::new(TimeSource::ValidFrom, Box::new(Exp::new(Duration::from_secs(3600))));
    rescore_recency(&mut hits_a, at(now_ms), &cfg_a);

    let mut hits_b = vec![mk_hit(1, 1.0, now_ms - hour)];
    let cfg_b =
        RecencyConfig::new(TimeSource::ValidFrom, Box::new(Exp::new(Duration::from_secs(86_400))));
    rescore_recency(&mut hits_b, at(now_ms), &cfg_b);

    assert!(
        hits_a[0].score < hits_b[0].score,
        "shorter half-life should produce smaller score: a={}, b={}",
        hits_a[0].score,
        hits_b[0].score
    );
    // Shorter half-life: at exactly one half-life, score = 0.5.
    assert!((hits_a[0].score - 0.5).abs() < 1e-5);
}

#[test]
fn zero_valid_from_hits_keep_their_priors() {
    // Hits with Hlc::ZERO carry no recency signal — their priors must
    // pass through, so they end interleaved among stamped hits purely
    // by prior score.
    let now_ms: u64 = 50_000_000_000;
    let mut hits = vec![
        mk_hit(1, 0.10, 0),              // no signal, low prior
        mk_hit(2, 0.90, now_ms - 1_000), // recent, high prior
        mk_hit(3, 0.50, 0),              // no signal, mid prior
    ];
    let cfg = RecencyConfig::default();
    rescore_recency(&mut hits, at(now_ms), &cfg);
    // The unstamped hits keep 0.10 / 0.50. The stamped hit decays from
    // 0.90 by a microscopic factor (1s under a 7-day half-life ≈ no-op).
    // Expected order: 2 (~0.9) > 3 (0.5) > 1 (0.1).
    let ids: Vec<u8> = hits.iter().map(|h| h.id[0]).collect();
    assert_eq!(ids, vec![2, 3, 1]);
    assert!((hits[2].score - 0.10).abs() < 1e-6);
    assert!((hits[1].score - 0.50).abs() < 1e-6);
}
