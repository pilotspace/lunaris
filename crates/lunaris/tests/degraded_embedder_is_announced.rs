//! W0.7 successor — a degraded embedder must ANNOUNCE itself, not wait to be asked.
//!
//! `Lunaris::embedder_backend()` made degradation *queryable*; it stayed silent
//! by default, and a caller has to know to ask. The existing `tracing::warn!` on
//! the Noop path is real, but neither SDK installs a subscriber
//! (`grep -rn tracing_subscriber crates/lunaris-{py,ts}/src` returns nothing), so
//! for a `pip install lunaris` user it is emitted into a void — which is the
//! exact population the ledger names ("silent empty results").
//!
//! The contract under test is `lunaris::degradation_notice`, the pure decision
//! function behind the announcement:
//!
//!   * degraded + nobody collecting warnings -> emit (the SDK case)
//!   * degraded + a subscriber IS installed  -> stay quiet, the `warn!` covers it
//!   * healthy                               -> never emit
//!   * explicitly suppressed                 -> never emit
//!
//! Kept pure so it needs no `env::set_var` (edition 2024 makes that unsafe, and
//! a process-global write races every sibling in the same binary).

use lunaris::{DegradationNotice, EmbedderBackend, degradation_notice};

#[test]
fn a_degraded_backend_with_no_subscriber_is_announced() {
    let n = degradation_notice(EmbedderBackend::Noop, false, None);
    let DegradationNotice::Emit(msg) = n else {
        panic!("expected an announcement for a degraded backend, got {n:?}");
    };
    // The message has to be actionable, not just alarming: name the condition
    // AND at least one remedy the reader can act on.
    assert!(msg.contains("noop"), "message does not name the backend: {msg}");
    assert!(msg.contains("LUNARIS_EMBEDDER_GGUF"), "message names no remedy: {msg}");
}

#[test]
fn a_subscriber_owns_the_reporting_when_one_is_installed() {
    // A host that installed tracing already receives the `warn!`. Printing to
    // stderr as well would double-report, and worse, bypass its log routing.
    assert_eq!(degradation_notice(EmbedderBackend::Noop, true, None), DegradationNotice::Silent);
}

#[test]
fn a_healthy_backend_is_never_announced() {
    for b in
        [EmbedderBackend::LlamaCpp, EmbedderBackend::OpenAiRemote, EmbedderBackend::OllamaRemote]
    {
        assert_eq!(
            degradation_notice(b, false, None),
            DegradationNotice::Silent,
            "{b} is not degraded and must not be announced"
        );
    }
}

#[test]
fn unresolved_is_unknown_not_degraded() {
    // `Unresolved` means `open` never ran in this process — a `with_parts*` test
    // seam. Announcing it would make every such handle print a false alarm.
    assert_eq!(
        degradation_notice(EmbedderBackend::Unresolved, false, None),
        DegradationNotice::Silent
    );
}

#[test]
fn an_operator_can_suppress_the_announcement() {
    for raw in ["1", "true", "TRUE", "yes"] {
        assert_eq!(
            degradation_notice(EmbedderBackend::Noop, false, Some(raw)),
            DegradationNotice::Silent,
            "{raw:?} should suppress"
        );
    }
}

#[test]
fn suppression_keys_on_the_accepted_set_not_on_presence() {
    // An `is_some()` check would let `LUNARIS_SUPPRESS_DEGRADED_WARNING=0` — and
    // an empty string, which is what an unset-but-exported var looks like in a
    // shell — silence the one warning the user most needs.
    for raw in ["0", "false", "", "  ", "no", "off"] {
        assert!(
            matches!(
                degradation_notice(EmbedderBackend::Noop, false, Some(raw)),
                DegradationNotice::Emit(_)
            ),
            "{raw:?} must NOT suppress the announcement"
        );
    }
}

// ---------------------------------------------------------------------------
// Built != wired. Everything above tests the decision function; none of it
// proves `Lunaris::open` calls it. These two run the REAL production path in a
// fresh child process — fresh because both the backend resolution and the
// announce-once flag are process-global `OnceLock`s, so one process cannot show
// the announced and suppressed arms apart.
// ---------------------------------------------------------------------------

const CHILD_MARKER: &str = "LUNARIS_ANNOUNCE_CHILD_URL";

/// The child body. Runs `Lunaris::open` for real against the URL the parent
/// passes, with every embedder route closed so the backend resolves to Noop.
/// `#[ignore]` keeps it out of a normal run; the parent invokes it by name.
#[tokio::test]
#[ignore = "child process of the announce tests; run by name via CHILD_MARKER"]
async fn announce_child_opens() {
    let url = std::env::var(CHILD_MARKER).expect("parent must set the URL");
    let _ = lunaris::Lunaris::open(&url).await;
}

fn run_child(url: &str, suppress: Option<&str>) -> String {
    use std::process::Command;
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.args(["--ignored", "--exact", "announce_child_opens", "--nocapture"])
        .env(CHILD_MARKER, url)
        // Close every route the resolver can take, so the child genuinely
        // degrades rather than stepping to a remote that happens to be up.
        .env("LUNARIS_EMBEDDER_GGUF", "/nonexistent/announce-test.gguf")
        .env_remove("LUNARIS_EMBEDDER_DIR")
        .env_remove("LUNARIS_EMBEDDER_OLLAMA_URL")
        .env_remove("LUNARIS_EMBEDDER_OPENAI_URL")
        .env_remove("LUNARIS_EMBEDDER_OPENAI_API_KEY")
        .env_remove("LUNARIS_EMBEDDER_OPENAI_MODEL")
        .env_remove(lunaris::SUPPRESS_DEGRADED_WARNING_ENV);
    if let Some(v) = suppress {
        cmd.env(lunaris::SUPPRESS_DEGRADED_WARNING_ENV, v);
    }
    let out = cmd.output().expect("spawn child");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // libtest reports the run on STDOUT; the announcement lands on STDERR. A
    // child that never ran and a child that ran silently look identical, so
    // assert the run happened before reading anything into its silence.
    assert!(
        stdout.contains("announce_child_opens") && stdout.contains("1 passed"),
        "the child did not run the test at all — the announcement being absent \
         would prove nothing.\nstdout: {stdout}\nstderr: {stderr}"
    );
    stderr
}

#[tokio::test]
async fn open_announces_a_degraded_backend_and_suppression_silences_it() {
    let moon = match lunaris_test_harness::EphemeralMoon::spawn().await {
        Ok(m) => m,
        Err(e) => {
            lunaris_test_harness::strict_skip::note_unavailable(format!(
                "no ephemeral Moon for the announce test: {e}"
            ));
            return;
        }
    };

    let announced = run_child(moon.url(), None);
    assert!(
        announced.contains("embedder backend is 'noop'"),
        "Lunaris::open did not announce a degraded backend — the decision \
         function is built but not wired.\nstderr: {announced}"
    );

    // The same child, one env var apart. Without this arm the assertion above
    // passes against an unconditional `eprintln!`.
    let suppressed = run_child(moon.url(), Some("1"));
    assert!(
        !suppressed.contains("embedder backend is 'noop'"),
        "{}=1 did not silence the announcement.\nstderr: {suppressed}",
        lunaris::SUPPRESS_DEGRADED_WARNING_ENV
    );
}
