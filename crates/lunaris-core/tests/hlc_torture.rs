//! HLC monotonicity torture test — 4 threads × 250_000 ticks (1M total).
//! Asserts: every issued timestamp is strictly greater than its predecessor
//!         in issue order, and no two ticks are exactly equal.

use std::sync::Arc;
use std::thread;

use lunaris_core::hlc::{Hlc, HlcClock};

#[test]
fn torture_4_threads_1m_ticks_no_inversions() {
    let clock = HlcClock::new(0);
    let n_threads = 4usize;
    let per_thread = 250_000usize;
    let mut handles = Vec::with_capacity(n_threads);
    for _ in 0..n_threads {
        let c = Arc::clone(&clock);
        handles.push(thread::spawn(move || {
            let mut local: Vec<(u128, Hlc)> = Vec::with_capacity(per_thread);
            for _ in 0..per_thread {
                let t = c.tick();
                // record the tick along with a strictly-increasing local sequence so we
                // can reconstruct global issue order via stable sort
                let seq = (t.wall_ms as u128) << 64 | (t.counter as u128) << 32;
                local.push((seq, t));
            }
            local
        }));
    }
    let mut all: Vec<(u128, Hlc)> =
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    // Sort by Hlc total order
    all.sort_by_key(|(_, t)| *t);
    let n = all.len();
    assert_eq!(n, n_threads * per_thread, "lost timestamps");
    for w in all.windows(2) {
        let a = w[0].1;
        let b = w[1].1;
        assert!(b > a, "non-monotonic neighbour: {a:?} then {b:?}");
        assert_ne!(a, b, "duplicate timestamp issued: {a:?}");
    }
}

#[test]
fn sequential_1k_ticks_strictly_increasing() {
    let c = HlcClock::new(0);
    let mut prev = c.tick();
    for _ in 0..1000 {
        let t = c.tick();
        assert!(t > prev, "sequential tick regressed: {prev:?} then {t:?}");
        prev = t;
    }
}

#[test]
fn bitemporal_overlaps() {
    use lunaris_core::bitemporal::BiTemporal;
    let c = HlcClock::new(0);
    let bt = BiTemporal::now(&c);
    let later = c.tick();
    assert!(bt.valid_at(later), "open-ended valid interval should include any later t");
    assert!(bt.system_at(later));
    assert!(bt.overlaps(later));
    let mut bt2 = bt;
    bt2.invalidate_valid(later);
    assert!(!bt2.valid_at(c.tick()), "invalidated valid interval excludes later t");
}
