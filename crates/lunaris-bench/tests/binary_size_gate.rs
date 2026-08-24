//! N-04 D4 — release-binary-size CI gate.
//!
//! Asserts the `target/release/lunaris-server` artifact exists and is
//! at or below the **35 MiB** ceiling. Baseline at the time the gate
//! was installed is **27 MiB** (post-N-03 native cutover; pre-strip on
//! macOS, post-strip on Linux). The 8 MiB headroom absorbs a single
//! medium-sized dependency addition before forcing the gate to be
//! re-evaluated.
//!
//! ## How to run
//!
//! Build the release binary first, then invoke the test with the
//! `ci-perf-gates` feature:
//!
//! ```sh
//! cargo build --release --workspace --exclude lunaris-py --exclude lunaris-ts
//! cargo test -p lunaris-bench --features ci-perf-gates --test binary_size_gate -- --include-ignored
//! ```
//!
//! `scripts/check-binary-size.sh` wraps both commands for CI.
//!
//! Without `--features ci-perf-gates` the test is `#[ignore]`d so a
//! plain `cargo test --workspace` doesn't fail on dev boxes that have
//! never built the release binary.
//!
//! ## Why a test and not a build.rs check?
//!
//! Build-time checks can't see the release binary at the workspace
//! `target/` root (Cargo invariants forbid build-script reads of the
//! enclosing `target/`). A test reads the path directly and emits a
//! sensible failure message naming the path + measured size.

use std::path::PathBuf;

/// Hard ceiling. Baseline measurement is ~27 MiB; an additional 8 MiB
/// allows for a single medium-sized dep without breaking CI.
const CEILING_BYTES: u64 = 35 * 1024 * 1024;

fn release_binary_path() -> PathBuf {
    // `CARGO_TARGET_DIR` moves build output, so a hardcoded `<workspace>/target`
    // diverges from where `cargo build --release` actually wrote the binary and
    // the gate fails with "missing release binary" instead of measuring one.
    // That is not hypothetical: this repo's own working convention sets the
    // variable, and `scripts/check-binary-size.sh` had to be run with
    // `env -u CARGO_TARGET_DIR` before this fix. Read at RUNTIME (`env::var`),
    // not compile time — a cached test binary must not bake in the path.
    //
    // Falls back to the workspace target root resolved relative to this crate's
    // manifest dir (`crates/lunaris-bench/`). Portable across host OSes — the
    // executable suffix on Windows is `.exe` (not a supported CI target for the
    // v0.4 native stack, but we resolve correctly anyway).
    let mut p = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let mut w = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            w.pop(); // crates/
            w.pop(); // workspace root
            w.push("target");
            w
        }
    };
    p.push("release");
    if cfg!(windows) {
        p.push("lunaris-server.exe");
    } else {
        p.push("lunaris-server");
    }
    p
}

#[test]
#[cfg_attr(
    not(feature = "ci-perf-gates"),
    ignore = "run with --features ci-perf-gates after `cargo build --release -p lunaris-server`"
)]
fn lunaris_server_release_binary_under_35_mib() {
    let bin = release_binary_path();
    let meta = std::fs::metadata(&bin).unwrap_or_else(|e| {
        panic!(
            "missing release binary at {}: {e}. Build it first: \
             `cargo build --release -p lunaris-server`. \
             scripts/check-binary-size.sh wraps both steps.",
            bin.display()
        )
    });
    let size = meta.len();
    let mib = size as f64 / (1024.0 * 1024.0);
    eprintln!(
        "lunaris-server release binary: {size} bytes ({mib:.2} MiB) — ceiling {} bytes (35 MiB)",
        CEILING_BYTES
    );
    assert!(
        size <= CEILING_BYTES,
        "release binary at {} is {size} bytes ({mib:.2} MiB) — exceeds 35 MiB ceiling. \
         See docs/benchmarks/v0.4-binary-size.md for the budget table and remediation playbook.",
        bin.display()
    );
}
