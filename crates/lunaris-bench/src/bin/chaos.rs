//! Plan 04-03 D-23 + Plan 12-04 — chaos subprocess.
//!
//! Two invocation shapes:
//!
//! ## Phase 4 / v0.1.0 — `chaos <storage_url> --corpus <N>` (unchanged)
//!
//! Spawned by `crates/lunaris-conformance/tests/crash_recovery.rs` via
//! `std::process::Command`. Opens a [`lunaris::Lunaris`] handle for the given
//! `<storage_url>` and issues ONE large `atomic_write` of `<N>` synthetic
//! primitives via [`lunaris_bench::build_chaos_corpus`].
//!
//! The parent test then sleeps an adversarially-chosen delay (`proptest`
//! uniform over `[1ms, atomic_write_p99]`) before sending `SIGKILL` via
//! `nix::sys::signal::kill`.
//!
//! ## Phase 12 Plan 12-04 — `chaos --profile helios` (new)
//!
//! Env-driven driver for the Helios tool-flow SIGKILL harness. Extends the
//! Phase 4 fault-injection site list with two Phase-12-specific sites:
//!
//! - `MidHeliosScratchpadWrite` — begin `HeliosScratchpad::write`, signal
//!   `ready\n`, sleep `KILL_OFFSET_NS`, land the write if not yet killed.
//!   Stresses the `atomic_write` single-call invariant (v0.1.0 Plan 04-02
//!   D-11) on the Helios path.
//! - `MidConsolidatorPromotion` — enable Consolidator-scope `helios:fs/`,
//!   write a baseline note so the pipeline has a promotion candidate,
//!   signal `ready\n`, sleep `KILL_OFFSET_NS`, then trigger a second
//!   `HeliosScratchpad::write` under the SAME scope (the write publishes a
//!   `ConsolidateEvent`; the race window is between publish and
//!   atomic_write fanout at the worker's flush).
//!
//! Required env:
//!
//! * `LUNARIS_URL` — `moon://host:port` or `postgres://...`.
//! * `KILL_SITE` — `MidHeliosScratchpadWrite` | `MidConsolidatorPromotion`.
//!
//! Optional env:
//!
//! * `LUNARIS_HELIOS_SESSION_ID` — session id the HeliosScratchpad binds
//!   to. Defaults to a fresh `session-chaos-<ulid>` if unset so multiple
//!   concurrent iters don't collide.
//! * `KILL_OFFSET_NS` — nanoseconds to sleep after emitting `ready\n`,
//!   before attempting to land the operation. Default 0 (maximizes the
//!   race window against parent's SIGKILL).
//!
//! Exit codes:
//!
//! - `0` — operation completed (commit landed before SIGKILL arrived OR
//!   after the post-ready sleep finished uninterrupted).
//! - `1` — `Lunaris::open` / setup failed (backend unreachable etc.). The
//!   parent surfaces this as a setup error, NOT a chaos signal.
//! - `2` — bad CLI args / missing env / unknown kill site.
//!
//! NB: when SIGKILL'd, this process never reaches the normal exit. The
//! parent observes the kill via `child.wait()` and proceeds to the fsck
//! assertion. That is the success path for the chaos test.
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` — no FFI here; SIGKILL is sent by the
//!   parent test using `nix`'s safe wrapper around `kill(2)`.
//! - All stderr / stdout writes go through `eprintln!` / `println!` — no
//!   raw `libc::write`.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Phase 12 dispatch: `--profile helios` short-circuits the positional
    // Phase-4 contract. Keep the Phase-4 arg shape strictly backward-compat:
    // chaos <url> --corpus <N>   (4 args incl. argv[0]).
    if args.iter().any(|a| a == "--profile") {
        return run_profile_dispatch(&args).await;
    }

    // -----------------------------------------------------------------------
    // Phase 4 legacy path (unchanged).
    // -----------------------------------------------------------------------
    run_phase4_legacy(&args).await
}

async fn run_phase4_legacy(args: &[String]) -> ExitCode {
    if args.len() < 4 || args.get(2).map(|s| s.as_str()) != Some("--corpus") {
        eprintln!("usage: chaos <storage_url> --corpus <N>");
        eprintln!("       chaos --profile helios        (Phase 12 HELIOS-06)");
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

    let lunaris = match lunaris::Lunaris::open(url).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Lunaris::open({url}) failed: {e}");
            return ExitCode::from(1);
        }
    };

    eprintln!("chaos: opened Lunaris handle to {url}; writing {count} primitives via atomic_write");

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

// ---------------------------------------------------------------------------
// Phase 12 Plan 12-04 — `--profile helios` dispatch
// ---------------------------------------------------------------------------

async fn run_profile_dispatch(args: &[String]) -> ExitCode {
    // Find the profile argument.
    let profile = args
        .iter()
        .position(|a| a == "--profile")
        .and_then(|i| args.get(i + 1).cloned());

    match profile.as_deref() {
        Some("helios") => run_helios_profile().await,
        Some(other) => {
            eprintln!("chaos: unknown --profile `{other}` (expected: helios)");
            ExitCode::from(2)
        }
        None => {
            eprintln!("chaos: --profile requires an argument (expected: helios)");
            ExitCode::from(2)
        }
    }
}

async fn run_helios_profile() -> ExitCode {
    // Required env.
    let url = match std::env::var("LUNARIS_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("chaos: --profile helios requires LUNARIS_URL env (moon://... | postgres://...)");
            return ExitCode::from(2);
        }
    };
    let kill_site_raw = match std::env::var("KILL_SITE") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!(
                "chaos: --profile helios requires KILL_SITE env \
                 (MidHeliosScratchpadWrite | MidConsolidatorPromotion)"
            );
            return ExitCode::from(2);
        }
    };

    // Optional env.
    let session_id = std::env::var("LUNARIS_HELIOS_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("session-chaos-{}", ulid::Ulid::new()));
    let kill_offset_ns: u64 = std::env::var("KILL_OFFSET_NS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    eprintln!(
        "chaos: --profile helios url={url} session_id={session_id} \
         kill_site={kill_site_raw} kill_offset_ns={kill_offset_ns}"
    );

    // Open Lunaris.
    let lunaris = match lunaris::Lunaris::open(&url).await {
        Ok(l) => Arc::new(l),
        Err(e) => {
            eprintln!("Lunaris::open({url}) failed: {e}");
            return ExitCode::from(1);
        }
    };

    // Construct HeliosScratchpad.
    let pad = lunaris::HeliosScratchpad::new(lunaris.clone(), &session_id);

    // If the kill-site covers a Consolidator race, enable scope-ON.
    // Idempotent + safe on repeat calls.
    if kill_site_raw == "MidConsolidatorPromotion" {
        lunaris.consolidator_pipeline().enable_for_scope("helios:fs/");
        // Seed one baseline note so the consolidator has a candidate
        // episode to race against.
        if let Err(e) = pad.write("seed.md", "seed content for consolidator race").await {
            eprintln!("chaos: helios profile baseline write failed: {e}");
            return ExitCode::from(1);
        }
    }

    // Ready signal — parent reads this line to decide when to start the
    // SIGKILL timer.
    println!("ready");
    if let Err(e) = std::io::stdout().flush() {
        eprintln!("chaos: stdout flush failed after ready: {e}");
        return ExitCode::from(1);
    }

    // Post-ready sleep. The parent races its SIGKILL against this sleep +
    // the subsequent operation.
    if kill_offset_ns > 0 {
        tokio::time::sleep(Duration::from_nanos(kill_offset_ns)).await;
    }

    // Dispatch the site-specific operation.
    match kill_site_raw.as_str() {
        "MidHeliosScratchpadWrite" => {
            // Single large-ish write — exercises HeliosScratchpad::write →
            // WorkingMemory::write → Lunaris::ingest → atomic_write.
            // Payload is ~4 KB to widen the race window.
            let payload = "chaos payload line; ".repeat(200);
            match pad.write("chaos-target.md", payload).await {
                Ok(_lsn) => {
                    println!("completed");
                    let _ = std::io::stdout().flush();
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("chaos: MidHeliosScratchpadWrite write failed: {e}");
                    ExitCode::from(1)
                }
            }
        }
        "MidConsolidatorPromotion" => {
            // Second write under the Consolidator-ON scope — the write
            // commits the episode, publishes a ConsolidateEvent, and the
            // worker's flush races the SIGKILL.
            let payload = "promotion candidate payload".to_string();
            match pad.write("chaos-promotion.md", payload).await {
                Ok(_lsn) => {
                    // Drain briefly so the consolidator worker has a
                    // chance to pick up the event before the parent kills.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    println!("completed");
                    let _ = std::io::stdout().flush();
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("chaos: MidConsolidatorPromotion write failed: {e}");
                    ExitCode::from(1)
                }
            }
        }
        other => {
            eprintln!(
                "chaos: unknown KILL_SITE `{other}` \
                 (expected MidHeliosScratchpadWrite | MidConsolidatorPromotion)"
            );
            ExitCode::from(2)
        }
    }
}
