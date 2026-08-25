//! W4.10 — cross-SDK embedder parity: the Python and TypeScript bindings must
//! produce **bit-identical** vectors for a fixed corpus.
//!
//! ## What this catches that nothing else does
//!
//! Both SDKs link the SAME Rust embedder, so any difference between them is a
//! difference introduced by the binding layer, not by the model:
//!
//! * **codegen-vs-handwritten divergence** — one SDK's glue converting through
//!   a different path than the other's;
//! * **locale-dependent tokenisation** — the corpus deliberately mixes ASCII,
//!   accented Latin, CJK and emoji, plus an empty string and a whitespace-only
//!   string;
//! * **FFI float corruption** — the family that produced the silent-zero-vectors
//!   P0, where one SDK shipped all-zero vectors and nothing noticed.
//!
//! Because it is a COMPARISON and not a golden file, it is independent of the
//! GGUF build, the llama.cpp version and the host — there is no committed
//! matrix to go stale. What is committed is the 100 fixed INPUTS, so both sides
//! are provably fed the same text.
//!
//! ## Why exit codes and not string matching
//!
//! The predecessor (`sdk_embedder_parity.rs`, deleted) returned `Ok(())` when
//! an interpreter was missing — so even wired up it would have reported green
//! having tested nothing. Each probe script here exits with a code that says
//! WHICH precondition failed (3 = built without `bindings-it`, 4 = no embedder
//! model), and this driver turns each into a NAMED skip through
//! `strict_skip::note_unavailable` — which panics under
//! `LUNARIS_CONFORMANCE_STRICT=1`. A non-zero exit that is neither 3 nor 4 is a
//! FAILURE, never a skip.
//!
//! ## Which embedder
//!
//! Both probes embed through `embedderConfigFromEnv` — the production
//! GGUF → remote → Noop resolver `Lunaris::open` walks — not through
//! `llamacpp()`. `EmbedderConfig` exposes no remote factory, so a
//! llamacpp-only probe could run ONLY where a GGUF is staged, which is no CI
//! runner: it would report "skipped" forever, and a permanent skip reads
//! exactly like a passing check. Via the resolver, one probe covers
//! llama.cpp locally and the stub OpenAI embedder in CI. A Noop resolution
//! is caught behaviourally (all-zero vectors) and classified as a skip,
//! because two Noops agree perfectly.
//!
//! ## Running it
//!
//! ```text
//! # Both SDKs must be built WITH `bindings-it`. No GGUF is required — set
//! # LUNARIS_EMBEDDER_OPENAI_URL to use a remote embedder instead.
//! (cd crates/lunaris-py && maturin develop --release --features bindings-it)
//! (cd crates/lunaris-ts && npx napi build --platform --release --features bindings-it)
//! cargo test -p lunaris-conformance --features bindings-it \
//!     --test run_sdk_embedder_parity -- --nocapture
//! ```
//!
//! NOTE: `napi build --features bindings-it` REWRITES the committed
//! `crates/lunaris-ts/index.{js,d.ts}` to include the test-only hooks. Do not
//! commit that — `lunaris-codegen`'s `shipped_dts_matches_a_real_build`
//! catches it (F24). The probe reads the hooks from the `.node` when the
//! committed entry lacks them, so a clean tree still runs this test.

#![cfg(feature = "bindings-it")]
#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Exit code a probe uses for "this SDK was built without `bindings-it`".
const EXIT_NO_BINDINGS_IT: i32 = 3;
/// Exit code a probe uses for "no embedder model is reachable".
const EXIT_NO_EMBEDDER: i32 = 4;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).unwrap().to_path_buf()
}

fn fixture(name: &str) -> PathBuf {
    workspace_root().join("crates/lunaris-conformance/fixtures/sdk_parity").join(name)
}

/// The Python interpreter to drive, in order: `LUNARIS_PY`, then the venv
/// named by `VIRTUAL_ENV`, then the in-tree venv `maturin develop` creates.
///
/// `VIRTUAL_ENV` is not optional polish — CI creates its venv at the REPO
/// ROOT and exports it that way, so a check that only knew the in-tree path
/// found no interpreter and, under strict mode, failed the job. The ambient
/// `python3` is still deliberately NOT a fallback: it almost certainly has no
/// `lunaris` installed, and the resulting `ModuleNotFoundError` would read as
/// a parity failure rather than as a missing prerequisite.
fn python_bin() -> Option<PathBuf> {
    let explicit = std::env::var("LUNARIS_PY").ok().filter(|s| !s.trim().is_empty());
    if let Some(p) = explicit.map(PathBuf::from) {
        return p.exists().then_some(p);
    }
    let venv = std::env::var("VIRTUAL_ENV")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|v| PathBuf::from(v).join("bin/python"));
    if let Some(p) = venv.filter(|p| p.exists()) {
        return Some(p);
    }
    let in_tree = workspace_root().join("crates/lunaris-py/.venv/bin/python");
    in_tree.exists().then_some(in_tree)
}

/// The built napi binding to import. Defaults to `index.js` — the surface
/// `napi build` REGENERATES, not the hand-written `index.mjs` shim, whose
/// export list is maintained by hand and does not name the `bindings-it`
/// hooks. Pointing at the shim makes a fully capable binding report the
/// feature as missing, which the driver would turn into a skip.
fn ts_binding() -> Option<PathBuf> {
    let p = std::env::var("LUNARIS_TS_BINDING")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("crates/lunaris-ts/index.js"));
    p.exists().then_some(p)
}

/// Outcome of one probe: a matrix, or a NAMED reason there is none.
enum Probe {
    Matrix(Vec<Vec<f64>>),
    Unavailable(String),
}

fn run_probe(label: &str, mut cmd: Command, out: &Path) -> anyhow::Result<Probe> {
    let output = cmd.output()?;
    let code = output.status.code();
    let stderr = String::from_utf8_lossy(&output.stderr);
    match code {
        Some(0) => {}
        Some(c) if c == EXIT_NO_BINDINGS_IT => {
            // Two distinct causes now share this code: an SDK built without
            // `bindings-it`, and no SDK present at all (jobs that run this
            // driver with no SDK build step). Carry the probe's own marker
            // through so the skip line says WHICH — a skip that names the wrong
            // reason sends the next reader to rebuild something that is fine.
            return Ok(Probe::Unavailable(format!(
                "{label} has no embed-batch probe — built without `bindings-it`, or \
                 not built at all: {}",
                stderr.trim()
            )));
        }
        Some(c) if c == EXIT_NO_EMBEDDER => {
            return Ok(Probe::Unavailable(format!(
                "{label} could not open an embedder (no staged GGUF): {}",
                stderr.trim()
            )));
        }
        other => {
            anyhow::bail!(
                "{label} probe failed with exit {other:?} — this is NOT a skip.\n\
                 stderr:\n{stderr}"
            );
        }
    }
    let raw = std::fs::read_to_string(out)
        .map_err(|e| anyhow::anyhow!("{label} exited 0 but wrote no matrix to {out:?}: {e}"))?;
    let matrix: Vec<Vec<f64>> = serde_json::from_str(&raw)?;
    Ok(Probe::Matrix(matrix))
}

#[tokio::test(flavor = "multi_thread")]
async fn python_and_typescript_embed_bit_identically() -> anyhow::Result<()> {
    let inputs_path = fixture("inputs.json");
    let expected_rows: usize =
        serde_json::from_str::<Vec<String>>(&std::fs::read_to_string(&inputs_path)?)?.len();

    let Some(py) = python_bin() else {
        lunaris_test_harness::strict_skip::note_unavailable(
            "run_sdk_embedder_parity: no Python interpreter (set LUNARIS_PY to a venv \
             with the lunaris wheel installed)",
        );
        return Ok(());
    };
    let Some(binding) = ts_binding() else {
        lunaris_test_harness::strict_skip::note_unavailable(
            "run_sdk_embedder_parity: no built TypeScript binding (run `npx napi build \
             --platform --release --features bindings-it` in crates/lunaris-ts)",
        );
        return Ok(());
    };

    let tmp = std::env::temp_dir().join(format!("lunaris-sdk-parity-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let py_out = tmp.join("py.json");
    let ts_out = tmp.join("ts.json");

    // SEQUENTIAL, not concurrent, and not negotiable: llama.cpp on Metal
    // deadlocks when two processes hold the GPU at once. Running these in
    // parallel would hang the suite on exactly the machines that can run it.
    let mut c = Command::new(&py);
    c.arg(fixture("py_matrix.py")).arg(&inputs_path).arg(&py_out);
    let py_probe = run_probe("the Python SDK", c, &py_out)?;

    let mut c = Command::new("node");
    c.arg(fixture("ts_matrix.mjs")).arg(&binding).arg(&inputs_path).arg(&ts_out);
    let ts_probe = run_probe("the TypeScript SDK", c, &ts_out)?;

    let (py_m, ts_m) = match (py_probe, ts_probe) {
        (Probe::Matrix(a), Probe::Matrix(b)) => (a, b),
        (Probe::Unavailable(why), _) | (_, Probe::Unavailable(why)) => {
            lunaris_test_harness::strict_skip::note_unavailable(format!(
                "run_sdk_embedder_parity: {why}"
            ));
            return Ok(());
        }
    };

    // Shape first. A pair of empty matrices compares equal, and "0 == 0" is the
    // canonical way a parity test reports success having compared nothing.
    assert_eq!(
        py_m.len(),
        expected_rows,
        "the Python SDK returned {} rows for {expected_rows} inputs",
        py_m.len()
    );
    assert_eq!(
        ts_m.len(),
        expected_rows,
        "the TypeScript SDK returned {} rows for {expected_rows} inputs",
        ts_m.len()
    );
    let dim = py_m[0].len();
    assert!(dim > 0, "the Python SDK returned zero-width vectors");
    assert!(
        py_m.iter().all(|r| r.len() == dim) && ts_m.iter().all(|r| r.len() == dim),
        "ragged matrix: not every row has width {dim}"
    );

    // Not every vector may be all-zeros. A NoopEmbedder satisfies bit-identity
    // perfectly — both SDKs would agree on nothing but zeros, which is exactly
    // the silent-zero-vectors P0 this test descends from.
    let nonzero = py_m.iter().filter(|r| r.iter().any(|v| *v != 0.0)).count();
    assert!(
        nonzero > 0,
        "every Python vector is all-zero across {expected_rows} inputs — the probe ran \
         against a Noop embedder, and bit-identity against another Noop proves nothing"
    );

    // Bit-exact, not approximate. Both sides link the same Rust embedder, so
    // any difference at all is a binding-layer defect and a tolerance would
    // hide precisely the corruption this exists to find. `to_bits` also makes
    // NaN compare unequal to itself, which is the correct outcome here.
    for (i, (pr, tr)) in py_m.iter().zip(ts_m.iter()).enumerate() {
        for (j, (p, t)) in pr.iter().zip(tr.iter()).enumerate() {
            assert_eq!(
                p.to_bits(),
                t.to_bits(),
                "input[{i}] dim[{j}]: Python {p:?} != TypeScript {t:?}. Both SDKs link the \
                 same Rust embedder, so a difference here is a binding-layer defect \
                 (codegen divergence, locale-dependent tokenisation, or FFI float \
                 corruption), not a model difference."
            );
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!("sdk embedder parity: {expected_rows}x{dim} bit-identical, {nonzero} non-zero rows");
    Ok(())
}
