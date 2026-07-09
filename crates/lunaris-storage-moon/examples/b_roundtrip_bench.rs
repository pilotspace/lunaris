//! moon-v051-perf-exploit W2 Task 2 — throwaway A/B latency bench, NOT a
//! CI-gated test. Moon's `total_commands_processed` INFO counter turns out
//! to NOT be incremented for `GRAPH.*` dispatch (live-probed: a bare
//! `GRAPH.ADDNODE`/`GRAPH.QUERY` outside any TXN left the counter
//! unchanged, while a plain `SET` incremented it by 1 every time) — so a
//! server-side command counter can't prove the round-trip reduction for
//! graph writes. This bench proves it the empirical way instead: wall-clock
//! latency of repeated existing-node rewrites, run once against the OLD
//! 2-round-trip code (`git stash` this crate's `atomic.rs`) and once against
//! the NEW optimistic 1-round-trip code, on the SAME live Moon.
//!
//! Usage:
//!   MOON_URL=moon://localhost:7802 cargo run -p lunaris-storage-moon \
//!     --example b_roundtrip_bench --release
//!
//! A/B recipe used for the W2 summary:
//!   1. Run as-is (NEW code) — record mean/p50/p99 update latency.
//!   2. `git stash push -- crates/lunaris-storage-moon/src/atomic.rs`
//!   3. Re-run — record OLD code's numbers.
//!   4. `git stash pop` to restore the NEW code.

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_moon::MoonStorage;
use serde_json::json;
use std::time::Instant;

const ITERS: usize = 300;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

fn hex16(b: &[u8; 16]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn node(id: &[u8; 16], name: &str) -> WriteOp {
    WriteOp::GraphNode {
        graph: "ignored".into(),
        id: id.to_vec(),
        label: "Person".into(),
        props: json!({ "id_hex": hex16(id), "name": name, "type": "Person" }),
    }
}

#[tokio::main]
async fn main() {
    let storage = MoonStorage::connect(&url()).await.expect("connect to live Moon");
    let scope =
        Scope::new(format!("bench-{}", ulid::Ulid::new().to_string().to_lowercase())).unwrap();

    let id = [0x99u8; 16];
    // Seed once (create path) — not measured.
    storage.atomic_write(&scope, &[node(&id, "seed")]).await.expect("seed");

    // Measure ITERS sequential UPDATE writes on the SAME existing node —
    // exactly the code path Task 2 changed (optimistic SET-first vs the old
    // exists-check + update pair).
    let mut samples_us: Vec<u64> = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let start = Instant::now();
        storage
            .atomic_write(&scope, &[node(&id, &format!("rewrite-{i}"))])
            .await
            .expect("update write");
        samples_us.push(start.elapsed().as_micros() as u64);
    }

    samples_us.sort_unstable();
    let sum: u64 = samples_us.iter().sum();
    let mean = sum as f64 / samples_us.len() as f64;
    let p50 = samples_us[samples_us.len() / 2];
    let p99 = samples_us[(samples_us.len() * 99) / 100];
    let min = samples_us[0];
    let max = samples_us[samples_us.len() - 1];

    println!(
        "EXISTING-NODE REWRITE LATENCY over {ITERS} iters (Moon at {}): mean={mean:.1}us \
         p50={p50}us p99={p99}us min={min}us max={max}us",
        url()
    );
}
