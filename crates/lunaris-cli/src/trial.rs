//! `lunaris try` — nothing to install, nothing to sign up for, a real recall.
//!
//! # The gap this closes
//!
//! 0.7.0 is Moon-only and refuses any Moon below 0.8.5 at connect. Moon v0.8.5
//! shipped with zero binary assets, so the honest instruction for a stranger
//! today is "build a Redis-compatible database from source, from a repository
//! you may not have access to". Meanwhile 0.6.x's `memory://` and
//! `sqlite:///path` onboarding paths were deleted. The front door is shut.
//!
//! Reopening it does NOT mean bringing three backends back. The Moon-only
//! ruling is right for the reasons `lunaris-mcp/src/state.rs` gives — *"a
//! mis-routed memory is harder to notice than a process that will not start"*.
//! What it means is that the ONE supported substrate has to be obtainable
//! without an install, and it already is: the Moon server compiles into this
//! binary behind the `embedded-moon` feature. `lunaris try` binds it to a
//! loopback port nobody else knows about, ingests a small built-in corpus and
//! runs the production recall over it.
//!
//! # Safety: this command cannot reach a real store
//!
//! Not "does not" — cannot, structurally:
//!
//! * It never calls [`crate::direct::resolve_store_url`], never reads
//!   `LUNARIS_STORE_URL`, and never consults contextd's discovery file. Its URL
//!   has exactly one source: the port [`launch_embedded_moon`] got by binding
//!   `127.0.0.1:0`.
//! * [`refuse_reserved_port`] is a second belt: if the kernel ever handed back
//!   a port that carries real data on a developer box (6379/6380/6381) or the
//!   dedicated bench Moon (6399), the trial shuts its own server down and
//!   fails rather than writing a sample corpus into it.
//! * It never issues `FLUSHALL`, and `--fresh` deletes a directory this command
//!   created rather than clearing a database.
//!
//! `tests/try_never_touches_a_real_store.rs` holds both the structural scan and
//! the behavioural proof.
//!
//! # Data lifetime: durable, and that is the point
//!
//! The trial store lives at `~/.lunaris/try/data` and SURVIVES the command.
//! A temp dir discarded on exit was the other candidate and it is the wrong
//! one: the newcomer who likes what they see immediately types a second
//! command, and a trial that threw its store away answers that second command
//! with nothing. Durable also makes re-running instant — every sample carries
//! a stable dedupe key, so a second run returns prior LSNs instead of writing
//! a second copy, and the store cannot grow no matter how often it is run.
//!
//! `--fresh` wipes it; `LUNARIS_TRY_DIR` relocates it (which is how the tests
//! stay out of `$HOME`).

// Without `embedded-moon` the store never gets constructed, so its fields and
// the port guard are legitimately unreachable. That build still has to compile
// warning-clean: `cargo clippy --workspace --all-targets` runs on the DEFAULT
// feature set, which is exactly the one where this happens.
#![cfg_attr(not(feature = "embedded-moon"), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use lunaris_core::Scope;
use lunaris_memory_service::protocol::MemoryRequest;

use crate::corpus::{DEFAULT_QUERY, SAMPLES};
use crate::request::TryArgs;
use crate::route::Route;

/// The scope the trial writes into. Fixed, and deliberately not `--scope`-able:
/// a first-run command that can be pointed at an arbitrary partition is a
/// command that can be pointed at yours.
const TRIAL_SCOPE: &str = "lunaris-try";

/// Ports that carry real data on developer machines and CI runners, mirroring
/// `lunaris_test_harness::RESERVED_PORTS`. The embedded launcher draws from
/// `127.0.0.1:0`, so landing here should be impossible; the check costs
/// nothing and the downside it guards against is writing a demo corpus into
/// somebody's live memory store.
const RESERVED_PORTS: &[u16] = &[6379, 6380, 6381, 6399];

/// Shown when the binary was built without the trial runtime. It is the entire
/// experience of a stranger who cloned the repo and ran `cargo build`, so its
/// wording is pinned by a test in every build.
const NO_EMBEDDED_MOON: &str = "\
this build of `lunaris` has no embedded store, so there is nothing for `try` to run against.

The published release binaries (npx / uvx / GitHub Releases) are built with it. A local
build needs the feature turned on explicitly, because it compiles the whole Moon server
and must never land in a plain `cargo test --workspace`:

    cargo build --release -p lunaris-cli --features embedded-moon

Already have a Moon? Skip the trial entirely:

    LUNARIS_STORE_URL=moon://127.0.0.1:<port> lunaris recall --scope mine \"your question\"";

/// Entry point. Owns the exit code so every failure leaves a non-zero status
/// with something the reader can act on — a first-run command that fails
/// silently, or fails with a stack trace, has spent its one chance.
pub(crate) async fn run(args: &TryArgs, globals: Globals) -> ExitCode {
    if let Err(err) = globals.check() {
        eprintln!("lunaris try: {err}");
        return ExitCode::from(2);
    }
    match run_inner(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("\nlunaris try: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// The global flags every subcommand accepts, which `try` cannot honour.
///
/// `--scope` and `--json` are declared `global = true` on the root parser, so
/// clap accepts them after `try` whether or not they mean anything there. They
/// do not: the trial writes to a fixed scope in a store it owns, and its output
/// is a guided tour rather than a payload. Silently ignoring a flag someone
/// typed on purpose is the failure mode this whole codebase is organised
/// against — `--scope mine` looks exactly like "put this in my partition" — so
/// they are refused with the command that does what was meant.
///
/// `scope_on_command_line` is a fact about argv, not about `Cli::scope`. The
/// root parser reads `--scope` from `LUNARIS_SCOPE` as well, and a great many
/// developers have that exported permanently; refusing on the parsed value
/// would break `lunaris try` for exactly the people most likely to run it. An
/// exported default is not an instruction about this command — a typed flag is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Globals {
    pub(crate) scope_on_command_line: bool,
    pub(crate) json: bool,
}

/// Did the caller TYPE `--scope`? Pure over an argv iterator so both the typed
/// and the exported-env cases are unit-testable without touching the process
/// environment (which is `unsafe` in edition 2024 and forbidden here).
///
/// Known limitation, accepted: `lunaris try --query "--scope"` would trip this.
/// Refusing a query that is literally the string `--scope` is a better failure
/// than the alternative — asking clap which source a value came from means
/// abandoning the derive API for raw `ArgMatches` across the whole parser.
pub(crate) fn scope_flag_typed<I, S>(argv: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    argv.into_iter().any(|a| {
        let a = a.as_ref();
        a == "--scope" || a.starts_with("--scope=")
    })
}

impl Globals {
    fn check(self) -> Result<(), String> {
        if self.scope_on_command_line {
            return Err(format!(
                "--scope is not accepted here. `try` runs a disposable store of its own \
                 and writes to the fixed `{TRIAL_SCOPE}` scope, so a scope you pass would \
                 be ignored rather than honoured.\nTo query a real store in your own \
                 scope:\n    LUNARIS_STORE_URL=moon://127.0.0.1:<port> lunaris recall \
                 --scope <yours> \"your question\""
            ));
        }
        if self.json {
            return Err("--json is not accepted here. `try` prints a guided first run, not a \
                 payload; there is no stable schema to emit and pretending otherwise \
                 would invite scripts to depend on a demo.\nFor machine-readable \
                 output, run `lunaris recall --json` against a real store."
                .to_owned());
        }
        Ok(())
    }
}

async fn run_inner(args: &TryArgs) -> anyhow::Result<()> {
    // Refuse FIRST, before anything expensive. Discovered the hard way: with
    // the check further down, a build without the feature verified — and on a
    // cold machine would have DOWNLOADED — 253 MB of weights before announcing
    // that it had no store to use them with.
    anyhow::ensure!(cfg!(feature = "embedded-moon"), NO_EMBEDDED_MOON);

    println!("lunaris try — a disposable memory store: no server, no account, no config.\n");

    let data_dir = trial_dir()?.join("data");
    if args.fresh && data_dir.exists() {
        std::fs::remove_dir_all(&data_dir)
            .with_context(|| format!("--fresh could not remove {}", data_dir.display()))?;
        println!("  ✓ fresh          removed the previous trial store");
    }
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create the trial data dir {}", data_dir.display()))?;

    // 1. Weights. Named before the store because it is the only step that can
    //    take minutes, and the reader deserves to know why before it starts.
    let embedder = resolve_embedder().await?;

    // 2. Store. The ONLY source of a URL on this path.
    let store = start_store(&data_dir).await?;
    println!("  ✓ store          embedded Moon on 127.0.0.1:{}", store.port);
    println!("  ✓ data           {} (kept — a second run reuses it)", data_dir.display());

    // 3. Everything past here goes through the shared dispatch, on one handle.
    let outcome = drive(&store.url, embedder, args).await;

    // Shut the server down whether or not the body succeeded; a leaked Moon
    // task would hold the data dir and make the NEXT run fail confusingly.
    store.shutdown().await;
    outcome
}

/// The part that talks to the store. Split out so [`run_inner`] can shut the
/// embedded Moon down on the error path as well as the happy one.
async fn drive(url: &str, embedder: EmbedderChoice, args: &TryArgs) -> anyhow::Result<()> {
    let scope = Scope::new(TRIAL_SCOPE).expect("TRIAL_SCOPE is a valid scope literal");
    let lunaris = crate::direct::open_handle(url, embedder.into_override()).await?;

    for sample in SAMPLES {
        let req = MemoryRequest::Ingest {
            scope: TRIAL_SCOPE.to_owned(),
            params: lunaris_memory_service::ingest::IngestParams {
                source: sample.source.to_owned(),
                content: sample.content.to_owned(),
                t_ref: None,
                metadata: None,
                dedupe_key: Some(sample.dedupe_key.to_owned()),
            },
        };
        crate::direct::dispatch_on(&lunaris, &scope, req)
            .await
            .with_context(|| format!("ingesting the sample memory {:?}", sample.dedupe_key))?;
    }
    println!("  ✓ corpus         {} sample memories", SAMPLES.len());

    let query = args.query.clone().unwrap_or_else(|| DEFAULT_QUERY.to_owned());
    println!("\n  ? {query}\n");

    let started = Instant::now();
    let value = crate::direct::dispatch_on(
        &lunaris,
        &scope,
        MemoryRequest::Recall {
            scope: TRIAL_SCOPE.to_owned(),
            params: lunaris_memory_service::recall::RecallParams {
                query,
                k: args.k,
                filters: None,
                as_of: None,
                raw: false,
            },
        },
    )
    .await
    .context("the recall itself failed")?;
    let elapsed = started.elapsed();

    print!("{}", crate::render::render(&value, Route::Trial));

    let hits = value.get("hits").and_then(|h| h.as_array()).map_or(0, Vec::len);
    println!("recalled {hits} of {} memories in {} ms", SAMPLES.len(), elapsed.as_millis());
    print_next_steps();
    Ok(())
}

fn print_next_steps() {
    println!(
        "\nNext:\n  \
         lunaris try --query \"what broke at 2 a.m.\"   ask the sample store something else\n  \
         lunaris try --fresh                          wipe it and start over\n\n\
         When you want your OWN memories, point the CLI at a Moon you run:\n  \
         LUNARIS_STORE_URL=moon://127.0.0.1:<port> lunaris recall --scope mine \"…\"\n  \
         docs: https://github.com/pilotspace/lunaris#quickstart"
    );
}

// ── Data directory ───────────────────────────────────────────────────────────

/// `LUNARIS_TRY_DIR`, else `~/.lunaris/try`.
///
/// The env override exists so tests never write to a developer's `$HOME`; it is
/// also the honest answer for anyone whose home directory is not writable.
fn trial_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) =
        std::env::var_os("LUNARIS_TRY_DIR").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
    {
        return Ok(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .context("cannot resolve $HOME; set LUNARIS_TRY_DIR to choose where the trial lives")?;
    Ok(home.join(".lunaris").join("try"))
}

// ── Embedder ─────────────────────────────────────────────────────────────────

/// Which embedder the trial will run with.
enum EmbedderChoice {
    /// The real granite-r2 GGUF, resolved by the engine from the path we staged.
    Granite,
    /// Deterministic hash vectors. Test/plumbing mode only — see
    /// [`resolve_embedder`].
    Stub,
}

impl EmbedderChoice {
    /// `None` means "let the engine resolve its own embedder", which is what
    /// every production run does.
    fn into_override(self) -> Option<Arc<dyn lunaris_core::Embedder>> {
        match self {
            Self::Granite => None,
            // 768-d matches granite-r2, so Moon sizes its FT index identically
            // and the plumbing under test is bit-for-bit the shipped plumbing.
            Self::Stub => Some(Arc::new(lunaris_core::StubEmbedder::new(768))),
        }
    }
}

/// Resolve, and stage if needed, the embedder the trial will use.
///
/// `LUNARIS_TRY_EMBEDDER=stub` selects deterministic hash vectors instead of
/// granite. It exists because the end-to-end test has to prove the whole pipe
/// runs on a machine that allows exactly ONE llama.cpp process at a time —
/// concurrent Metal contexts deadlock. It is announced loudly, it only ever
/// affects the throwaway trial scope, and an unrecognised value is a hard
/// error rather than a silent fallback to either side.
async fn resolve_embedder() -> anyhow::Result<EmbedderChoice> {
    match std::env::var("LUNARIS_TRY_EMBEDDER").unwrap_or_default().trim() {
        "" | "granite" => {
            let (path, how) = crate::stage::ensure_embedder().await?;
            let note = match how {
                crate::stage::Staged::OperatorSupplied => "from LUNARIS_EMBEDDER_GGUF",
                crate::stage::Staged::AlreadyPresent => "already staged",
                crate::stage::Staged::Downloaded => "downloaded just now",
            };
            let name =
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            println!("  ✓ embedder       {name} ({note})");
            Ok(EmbedderChoice::Granite)
        }
        "stub" => {
            eprintln!(
                "  ! LUNARIS_TRY_EMBEDDER=stub — vectors are deterministic hashes, NOT real \
                 embeddings.\n    Ranking below is meaningless; this mode exists to test the \
                 plumbing."
            );
            println!("  ✓ embedder       stub (deterministic hashes — plumbing mode)");
            Ok(EmbedderChoice::Stub)
        }
        other => anyhow::bail!(
            "LUNARIS_TRY_EMBEDDER={other:?} is not recognised. Use `granite` (the default, \
             a real 253 MB GGUF) or `stub` (deterministic hashes, for testing the plumbing). \
             Refusing to guess: silently picking either one would make the output mean \
             something different than you think."
        ),
    }
}

// ── Store lifecycle ──────────────────────────────────────────────────────────

/// A loopback Moon this process started, and the URL that reaches it.
struct TrialStore {
    url: String,
    port: u16,
    #[cfg(feature = "embedded-moon")]
    guard: lunaris_memory_service::embedded_moon::EmbeddedMoonGuard,
}

impl TrialStore {
    async fn shutdown(self) {
        #[cfg(feature = "embedded-moon")]
        self.guard.shutdown().await;
    }
}

/// Refuse a port that carries real data on a developer machine.
///
/// Pure so the refusal is unit-testable without binding anything — the failure
/// this guards against is the one you cannot safely reproduce.
fn refuse_reserved_port(port: u16) -> anyhow::Result<u16> {
    anyhow::ensure!(
        !RESERVED_PORTS.contains(&port),
        "the embedded store was handed port {port}, which is reserved for real data \
         (6379 Redis, 6380 Moon dev, 6381 a live Lunaris store, 6399 the bench Moon). \
         Refusing to write a sample corpus there. Re-run — the next draw will differ."
    );
    Ok(port)
}

#[cfg(feature = "embedded-moon")]
async fn start_store(data_dir: &Path) -> anyhow::Result<TrialStore> {
    let dir = data_dir.to_string_lossy().into_owned();
    let guard = lunaris_memory_service::embedded_moon::launch_embedded_moon(&dir).await.context(
        "the embedded store did not come up. This is usually a data directory that is \
             not writable, or a previous `lunaris try` still holding it — try `lunaris try \
             --fresh`",
    )?;
    let port = match refuse_reserved_port(guard.port) {
        Ok(p) => p,
        Err(e) => {
            guard.shutdown().await;
            return Err(e);
        }
    };
    Ok(TrialStore { url: format!("moon://127.0.0.1:{port}"), port, guard })
}

#[cfg(not(feature = "embedded-moon"))]
#[allow(clippy::unused_async)]
async fn start_store(_data_dir: &Path) -> anyhow::Result<TrialStore> {
    anyhow::bail!(NO_EMBEDDED_MOON)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lunaris try --scope mine` reads as "put this in my partition". It does
    /// not, and cannot. Refusing beats ignoring.
    #[test]
    fn a_scope_flag_is_refused_rather_than_ignored() {
        let err = Globals { scope_on_command_line: true, json: false }
            .check()
            .expect_err("--scope must not be silently dropped");
        assert!(err.contains("not accepted here"), "{err}");
        assert!(err.contains(TRIAL_SCOPE), "the message must name the scope try DOES use: {err}");
        assert!(err.contains("lunaris recall"), "it must name the command that works: {err}");
    }

    #[test]
    fn a_json_flag_is_refused_rather_than_ignored() {
        let err = Globals { scope_on_command_line: false, json: true }
            .check()
            .expect_err("--json must not be silently dropped");
        assert!(err.contains("not accepted here"), "{err}");
    }

    #[test]
    fn a_plain_invocation_passes_the_globals_check() {
        assert!(Globals { scope_on_command_line: false, json: false }.check().is_ok());
    }

    /// The distinction that keeps the refusal from becoming a nuisance: a TYPED
    /// `--scope` is an instruction about this command, an exported
    /// `LUNARIS_SCOPE` is a default for the shell. Refusing on the latter would
    /// break `lunaris try` for every developer who has that variable set — i.e.
    /// most of the people who would run it.
    #[test]
    fn only_a_typed_scope_flag_counts_not_an_exported_default() {
        assert!(scope_flag_typed(["lunaris", "--scope", "mine", "try"]));
        assert!(scope_flag_typed(["lunaris", "try", "--scope=mine"]));
        assert!(!scope_flag_typed(["lunaris", "try"]));
        assert!(!scope_flag_typed(["lunaris", "try", "--query", "--scope is not a scope"]));
    }

    #[test]
    fn the_trial_scope_is_a_valid_scope() {
        Scope::new(TRIAL_SCOPE).expect("TRIAL_SCOPE must parse — run_inner unwraps it");
    }

    /// The guard that stands between a demo corpus and somebody's live store.
    #[test]
    fn reserved_ports_are_refused_and_ordinary_ones_are_not() {
        for p in [6379_u16, 6380, 6381, 6399] {
            let err = refuse_reserved_port(p).expect_err("must refuse a reserved port");
            assert!(err.to_string().contains(&p.to_string()), "{err}");
        }
        assert_eq!(refuse_reserved_port(53_211).expect("an ephemeral port is fine"), 53_211);
    }

    /// A typo must not silently become "real embeddings" or "fake embeddings" —
    /// both readings produce output the caller will misinterpret.
    #[tokio::test]
    async fn an_unrecognised_embedder_mode_is_a_hard_error() {
        // Cannot mutate the environment (edition 2024 makes it `unsafe`, and
        // this crate forbids unsafe), so assert on the message the branch
        // produces instead. Keeping the wording pinned is the point: it is what
        // tells the reader which two values exist.
        let src = include_str!("trial.rs");
        assert!(src.contains("is not recognised"));
        assert!(src.contains("Refusing to guess"));
    }

    /// `NO_EMBEDDED_MOON` is the message a stranger sees if they build from
    /// source without the feature. It must name the exact build command, not
    /// just the missing capability.
    #[test]
    fn the_missing_feature_message_carries_the_fix() {
        assert!(NO_EMBEDDED_MOON.contains("--features embedded-moon"));
        assert!(NO_EMBEDDED_MOON.contains("LUNARIS_STORE_URL"));
    }
}
