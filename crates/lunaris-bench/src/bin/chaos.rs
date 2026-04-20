//! Plan 04-03 D-23 — chaos subprocess for the crash-recovery property test.
//!
//! Spawned by `crates/lunaris-conformance/tests/crash_recovery.rs` via
//! `std::process::Command`. Opens a [`lunaris::Lunaris`] handle for the given
//! `<storage_url>` and issues ONE large `atomic_write` of `<N>` synthetic
//! primitives via [`lunaris_bench::build_chaos_corpus`].
//!
//! The parent test then sleeps an adversarially-chosen delay (`proptest`
//! uniform over `[1ms, atomic_write_p99]`) before sending `SIGKILL` via
//! `nix::sys::signal::kill`. The post-kill assertion in the parent verifies
//! `STORE-06` (atomic_write all-or-nothing under `kill -9`) AND `OPS-07`
//! (no orphan vector / graph rows after recovery) by reopening the SAME
//! storage URL and counting primitives via `scan_range`.
//!
//! ## Usage
//!
//! ```text
//! chaos <storage_url> --corpus <N>
//! ```
//!
//! Example:
//!
//! ```text
//! chaos moon://localhost:6390 --corpus 10000
//! chaos postgres://localhost/lunaris_chaos --corpus 10000
//! ```
//!
//! ## Exit codes
//!
//! - `0` — atomic_write completed before SIGKILL arrived (the all-or-nothing
//!   "ALL" branch on the parent's assertion).
//! - `1` — Lunaris::open failed OR build_chaos_corpus failed (the parent
//!   surfaces this as a backend connectivity / corpus generation problem,
//!   NOT a chaos test failure).
//! - `2` — bad CLI args.
//!
//! NB: when SIGKILL'd, this process never reaches `ExitCode::SUCCESS`; the
//! parent observes the kill via `child.wait()` and proceeds to the recovery
//! assertion. That is the success path for the chaos property test.
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` — no FFI here; the SIGKILL is sent by the
//!   parent test using `nix`'s safe wrapper around `kill(2)`.
//! - All stderr / stdout writes go through `eprintln!` / `println!` — no
//!   raw `libc::write` or stdout-handle lock manipulation.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Expected: chaos <url> --corpus <N>
    //          [0]    [1]   [2]       [3]
    if args.len() < 4 || args.get(2).map(|s| s.as_str()) != Some("--corpus") {
        eprintln!("usage: chaos <storage_url> --corpus <N>");
        eprintln!();
        eprintln!("examples:");
        eprintln!("  chaos moon://localhost:6390 --corpus 10000");
        eprintln!("  chaos postgres://localhost/lunaris_chaos --corpus 10000");
        return ExitCode::from(2);
    }

    let url = &args[1];
    let count: u64 = match args[3].parse() {
        Ok(n) if n > 0 => n,
        Ok(_) => {
            eprintln!("invalid corpus count `{}`: must be > 0", args[3]);
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("invalid corpus count `{}`: {e}", args[3]);
            return ExitCode::from(2);
        }
    };

    // Open the Lunaris handle. If the backend is unreachable, surface as
    // exit code 1 so the parent test can distinguish "couldn't connect" from
    // "kill -9 mid-write" (the former is a setup error, not a chaos signal).
    let lunaris = match lunaris::Lunaris::open(url).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Lunaris::open({url}) failed: {e}");
            return ExitCode::from(1);
        }
    };

    eprintln!("chaos: opened Lunaris handle to {url}; writing {count} primitives via atomic_write");

    // Issue ONE large atomic_write via build_chaos_corpus. The parent test
    // races a SIGKILL against this call; either we win (commit completes,
    // exit 0) or the parent wins (process killed mid-TXN, recovery branch
    // asserts zero rows).
    let written = match lunaris_bench::build_chaos_corpus(&lunaris, count, /*seed=*/ 0xC0FFEE, url)
        .await
    {
        Ok(n) => n,
        Err(e) => {
            eprintln!("build_chaos_corpus failed: {e:?}");
            return ExitCode::from(1);
        }
    };

    eprintln!("chaos: wrote {written} primitives via atomic_write to {url}");
    ExitCode::SUCCESS
}
