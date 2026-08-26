//! `lunaris try` must be structurally incapable of reaching a store it did not
//! start.
//!
//! This is the hardest constraint on the command. 6381 on the maintainer's box
//! is a live Lunaris memory store with well over a million keys; 6379 and 6380
//! are a stock Redis and an ai-proxy Redis; 6399 is the dedicated benchmark
//! Moon. A first-run demo that ingested six sample memories into any of them —
//! or, worse, that a user pointed at their production store by leaving an env
//! var exported — would be an unrecoverable first impression.
//!
//! Two layers of proof, because either alone is weak:
//!
//! 1. **Structural** (runs everywhere, no build features): read `src/trial.rs`
//!    and assert it contains none of the ways a Lunaris surface resolves a
//!    store URL from its environment. This catches the regression at review
//!    time, in the file where it would be written.
//! 2. **Behavioural** (needs `embedded-moon`): actually run the binary with
//!    `LUNARIS_STORE_URL` and `LUNARIS_CONTEXTD_SOCKET` pointed at reserved
//!    ports, and assert it succeeds anyway on a port of its own. This catches
//!    the case where the env is read somewhere the scan does not look.

use std::path::PathBuf;

fn src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Lines that are prose, not behaviour.
fn code_lines(body: &str) -> impl Iterator<Item = (usize, &str)> {
    body.lines().enumerate().filter(|(_, l)| {
        let t = l.trim_start();
        !t.starts_with("//") && !t.starts_with('*')
    })
}

/// Named ways a Lunaris surface discovers a store that already exists. Naming
/// any of them in `trial.rs` — even to "just check" — is the first step toward
/// a demo corpus landing in a live store.
const DISCOVERY_MECHANISMS: &[&str] =
    &["resolve_store_url", "store_discovery", "discover_contextd_moon", "contextd-moon.url"];

/// The complete list of environment variables `trial.rs` may read. An
/// allowlist rather than a denylist: a denylist has to predict the name of the
/// variable somebody adds next, and this one does not.
const ALLOWED_ENV_READS: &[&str] = &["LUNARIS_TRY_DIR", "LUNARIS_TRY_EMBEDDER", "HOME"];

#[test]
fn the_trial_never_resolves_a_store_from_the_environment() {
    let body = src("trial.rs");

    let mut offenders = Vec::new();
    for (i, line) in code_lines(&body) {
        for needle in DISCOVERY_MECHANISMS {
            if line.contains(needle) {
                offenders.push(format!("trial.rs:{}: {needle}", i + 1));
            }
        }
        // Every env read on a code line must name an allowed variable. The
        // quoted argument is on the same line in every form we use.
        if line.contains("env::var") {
            let named = ALLOWED_ENV_READS.iter().any(|v| line.contains(v));
            if !named {
                offenders.push(format!("trial.rs:{}: unlisted env read — {}", i + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "`lunaris try` learned how to find an existing store: {offenders:?}.\n\
         Its URL must have exactly ONE source — the port `launch_embedded_moon` \
         obtained by binding 127.0.0.1:0. Reading anything outside \
         {ALLOWED_ENV_READS:?} would let an exported variable aim a demo corpus \
         at a live memory store. If a new variable really belongs here, add it to \
         the allowlist in this test, deliberately."
    );
}

/// The positive half: the URL really does come from the launcher. Without this,
/// deleting the launcher call and hard-coding a URL would pass the scan above.
#[test]
fn the_trial_url_comes_from_the_embedded_launcher() {
    let body = src("trial.rs");
    assert!(
        code_lines(&body).any(|(_, l)| l.contains("launch_embedded_moon")),
        "trial.rs no longer calls launch_embedded_moon. If the trial gained a \
         different store source, this whole file needs rewriting deliberately — \
         the safety claim rests on the URL being a loopback port this process bound."
    );
    assert!(
        code_lines(&body).any(|(_, l)| l.contains("refuse_reserved_port")),
        "the reserved-port refusal is gone; the kernel handing back 6381 would \
         no longer be caught"
    );
}

/// `FLUSHALL` on a store the process did not start is the one irreversible
/// mistake available here. `--fresh` deletes a directory instead.
#[test]
fn the_trial_never_flushes_a_database() {
    // Every source file in the crate, not a hand-listed four. The list used to
    // name `stage.rs`, which W0.7 deleted when the model stager moved into
    // `lunaris-core` — and a list that has to be edited when a file is added
    // is a list that stops covering the module somebody writes next.
    let mut scanned = 0usize;
    for path in crate_sources() {
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        scanned += 1;
        assert!(
            !code_lines(&body).any(|(_, l)| l.contains("FLUSHALL") || l.contains("flushall")),
            "{} issues FLUSHALL. `--fresh` must delete the trial's own data \
             directory; clearing a database can hit one this command did not create.",
            path.display()
        );
    }
    // Instrument self-check: a walk that found nothing passes this test for
    // the wrong reason, and reads identically to a clean crate.
    assert!(scanned >= 4, "the source walk found only {scanned} files — the walk is broken");
}

/// Every `.rs` file under `crates/lunaris-cli/src/`, recursively.
fn crate_sources() -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
    out
}

// ── Behavioural proof ────────────────────────────────────────────────────────

/// Run the real binary with every "find my store" variable pointed at reserved
/// ports and assert it ignores all of them.
///
/// If `try` ever read one, this test would either fail (the connect is refused
/// or the version handshake rejects a non-Moon) or — far worse on a developer
/// box — succeed by writing into 6381. Asserting on the port it PRINTS is what
/// makes the pass meaningful rather than incidental.
#[test]
#[cfg(feature = "embedded-moon")]
fn try_ignores_every_store_variable_in_the_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lunaris"))
        .arg("try")
        .env("LUNARIS_TRY_DIR", dir.path())
        .env("LUNARIS_TRY_EMBEDDER", "stub")
        .env("LUNARIS_EMBEDDER_GGUF", "/nonexistent/try-safety.gguf")
        .env("HOME", dir.path())
        // Every one of these is a lie the command must not believe.
        .env("LUNARIS_STORE_URL", "moon://127.0.0.1:6381")
        .env("LUNARIS_CONTEXTD_SOCKET", std::path::Path::new("/nonexistent/contextd.sock"))
        .output()
        .expect("spawn the lunaris binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "`lunaris try` must succeed with a hostile environment — it does not read \
         any of it.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    for reserved in ["6379", "6380", "6381", "6399"] {
        assert!(
            !stdout.contains(&format!("127.0.0.1:{reserved}")),
            "`lunaris try` reported a connection to 127.0.0.1:{reserved}. That port \
             carries real data.\n{stdout}"
        );
    }
    assert!(
        stdout.contains("embedded Moon on 127.0.0.1:"),
        "the trial must say which port it started, so this assertion has something \
         to check and so the reader knows it is not their store\n{stdout}"
    );
}
