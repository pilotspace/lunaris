//! Deterministic fixture generator for the lunaris-hook cold-start latency gate.
//!
//! Generates NDJSON lines (one JSON object per line) suitable for use as the
//! `tests/fixtures/replay_1000.json` replay fixture. Each line is a valid
//! Claude Code hook envelope covering the four supported event kinds evenly.
//!
//! # Usage
//!
//! ```text
//! cargo run --example gen_replay_fixture -p lunaris-hook -- --count 1000 --seed 42 \
//!   > crates/lunaris-hook/tests/fixtures/replay_1000.json
//! ```
//!
//! Re-running with the same `--seed` produces the same output byte-for-byte.
//!
//! # Design
//!
//! Uses a hand-rolled LCG (Linear Congruential Generator) so there is no
//! dependency on the `rand` crate. The LCG is seeded by the `--seed` argument.
//! All 64-bit arithmetic uses wrapping operations to match the C standard.

fn main() {
    // Parse --count and --seed from std::env::args() using a simple match loop.
    let args: Vec<String> = std::env::args().collect();
    let mut count: usize = 1000;
    let mut seed: u64 = 42;
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--count" => {
                if let Some(val) = iter.next() {
                    count = val.parse().unwrap_or(1000);
                }
            }
            "--seed" => {
                if let Some(val) = iter.next() {
                    seed = val.parse().unwrap_or(42);
                }
            }
            _ => {}
        }
    }

    // Validate: count must be divisible by 4 for even kind distribution.
    // If not, round up to the next multiple of 4.
    if count % 4 != 0 {
        let rounded = ((count / 4) + 1) * 4;
        eprintln!("gen_replay_fixture: count {count} not divisible by 4; rounding up to {rounded}");
        count = rounded;
    }

    // Hand-rolled LCG for deterministic pseudo-random content variation.
    // Parameters from Knuth MMIX: multiplier 6364136223846793005, addend 1442695040888963407.
    let mut lcg = seed;
    let mut next_lcg = move || -> u64 {
        lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        lcg
    };

    // Four canonical kinds distributed evenly (count/4 each).
    // Interleaved round-robin to exercise the pipeline's kind-dispatch on each iteration.
    let kinds = ["PreToolUse", "PostToolUse", "Stop", "SessionStart"];
    let tool_names = ["Edit", "Write", "Bash"];
    let per_kind = count / 4;

    let mut lines: Vec<serde_json::Value> = Vec::with_capacity(count);

    for i in 0..count {
        let kind = kinds[i % 4];
        // Shard sessions so dedupe keys are distinct across envelopes.
        let session_idx = i / 4;
        let session_id = format!("session-{session_idx:08x}-{:08x}", next_lcg() & 0xFFFF_FFFF);
        let cwd_idx = (next_lcg() % 20) as usize;
        let project = format!("/home/dev/project-{cwd_idx}");
        // Timestamps spread across one day to exercise the timestamp field.
        let hour = (i * 24) / count;
        let minute = ((i * 24 * 60) / count) % 60;
        let second = ((i * 24 * 3600) / count) % 60;
        let ts = format!("2026-05-25T{hour:02}:{minute:02}:{second:02}Z");
        // Unique event_id per envelope — prevents cross-run dedupe collisions.
        let event_id = format!("evt-{i:04}-{:016x}", next_lcg());

        let envelope = match kind {
            "PreToolUse" => {
                let tool = tool_names[(next_lcg() % 3) as usize];
                let file_idx = (next_lcg() % per_kind as u64) as usize;
                serde_json::json!({
                    "hook_event_name": "PreToolUse",
                    "session_id": session_id,
                    "cwd": project,
                    "tool_name": tool,
                    "tool_input": {
                        "path": format!("src/module_{file_idx}.rs"),
                        "content": format!(
                            "fn func_{i}() -> u64 {{ {} }}", next_lcg()
                        )
                    },
                    "event_id": event_id,
                    "timestamp": ts,
                })
            }
            "PostToolUse" => {
                let tool = tool_names[(next_lcg() % 3) as usize];
                let file_idx = (next_lcg() % per_kind as u64) as usize;
                let output_ms = next_lcg() % 500;
                serde_json::json!({
                    "hook_event_name": "PostToolUse",
                    "session_id": session_id,
                    "cwd": project,
                    "tool_name": tool,
                    "tool_input": {
                        "path": format!("src/module_{file_idx}.rs")
                    },
                    "tool_response": {
                        "output": format!("completed in {output_ms}ms"),
                        "exit_code": 0
                    },
                    "event_id": event_id,
                    "timestamp": ts,
                })
            }
            "Stop" => {
                serde_json::json!({
                    "hook_event_name": "Stop",
                    "session_id": session_id,
                    "cwd": project,
                    "event_id": event_id,
                    "timestamp": ts,
                })
            }
            "SessionStart" | _ => {
                serde_json::json!({
                    "hook_event_name": "SessionStart",
                    "session_id": session_id,
                    "cwd": project,
                    "event_id": event_id,
                    "timestamp": ts,
                })
            }
        };
        lines.push(envelope);
    }

    // Write NDJSON to stdout — exactly `count` lines.
    for line in &lines {
        println!("{}", serde_json::to_string(line).expect("serialize envelope"));
    }
}
