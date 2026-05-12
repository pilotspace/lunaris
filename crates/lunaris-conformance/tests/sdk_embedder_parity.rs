//! Plan 21-03 Task 1 — Cross-SDK embedder parity test.
//!
//! Runs a fixed 100-input corpus through BOTH the Python (`lunaris-py`) and
//! TypeScript (`lunaris-ts`) SDKs using `EmbedderConfig.fastembed()` with the
//! same shared cache directory, captures two `100 × 768` f32 matrices, and
//! asserts byte-identical (bits-exact) equality. Catches codegen-vs-handwritten
//! divergence, accidental locale-dependent tokenization, and FFI-boundary
//! float corruption.
//!
//! ## Toolchain prerequisites
//!
//! This test orchestrates external interpreters; both MUST be present:
//!
//! - **Python 3.11+** with `lunaris` importable
//!   (build via `cd crates/lunaris-py && maturin develop --release`).
//! - **Node 20+** with `lunaris-ts` built
//!   (build via `cd crates/lunaris-ts && npm install && npm run build`).
//! - **fastembed ONNX weight cache** at `$LUNARIS_FASTEMBED_CACHE_DIR` OR a
//!   tmpdir-writable host with network access on first run (~600 MB pull).
//!
//! If either interpreter is missing, the test prints a skip message to stderr
//! and returns `Ok(())` — feature-gated invocation on a misconfigured host
//! should not false-fail.
//!
//! ## Invocation
//!
//! ```
//! # Default — test is NOT compiled or run.
//! cargo test -p lunaris-conformance
//!
//! # With both toolchains present:
//! cargo test -p lunaris-conformance --features sdk-parity-it sdk_embedder_parity
//! ```
//!
//! ## Binary format used by the fixtures
//!
//! Each fixture writes the embedded matrix to its `--out` path in this
//! exact layout:
//!
//! ```text
//! [u32 count_le][u32 dim_le][count * dim * 4 bytes f32_le row-major]
//! ```
//!
//! Both `embed_via_py.py` and `embed_via_ts.mjs` MUST honour this layout
//! verbatim; the Rust reader validates `count` and `dim` before iterating.
//!
//! ## Gate
//!
//! Behind the `sdk-parity-it` cargo feature so default `cargo test --workspace`
//! does not pull this in (heavyweight: loads the ONNX model twice, requires
//! Python + Node toolchains).

#![cfg(feature = "sdk-parity-it")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Fixed parity corpus — 100 hand-written strings, no PII, deterministic.
// ---------------------------------------------------------------------------

const PARITY_CORPUS: &[&str] = &[
    "hello world",
    "lunaris memory engine",
    "bi-temporal mvcc store",
    "agent memory retrieval",
    "fastembed gemma 300m onnx",
    "moon redis compatible substrate",
    "postgres backend portability proof",
    "semantic search over facts",
    "graph traversal opt-in",
    "bm25 keyword lookup",
    "byo onnx model from s3",
    "ollama embedder http api",
    "rust pyo3 napi sdk parity",
    "execution preference cpu coreml cuda",
    "pooling mode mean cls",
    "tokenizer json hf format",
    "cross encoder reranker bge",
    "noop reranker passthrough fallback",
    "retrieval dsl composable",
    "vector keyword graph fuse",
    "rrf reciprocal rank fusion",
    "filter as of point in time",
    "top k truncation",
    "scope multi agent rfc 0001",
    "episode builder ingest",
    "snapshot lsn monotonic",
    "consolidator pipeline flush",
    "graph pipeline default off",
    "config dict surface override",
    "env var resolution chain",
    "code surface authoritative",
    "first principle thinking",
    "ownership as architecture",
    "zero cost abstraction first",
    "error handling thiserror anyhow",
    "fearless concurrency message passing",
    "rwlock over mutex",
    "send sync bounds intentional",
    "tokio mpsc oneshot watch",
    "structured concurrency joinset",
    "semaphore bounded permits",
    "circuit breaker exponential backoff",
    "deadline propagation timeout",
    "rollback compensating actions",
    "observability tracing spans",
    "structured fields boundary log",
    "typestate compile time state machine",
    "newtype pattern user id cents",
    "phantom data zero size",
    "soa over aos cache friendly",
    "rayon par iter cpu bound",
    "criterion benchmark hot path",
    "release profile lto fat codegen unit",
    "strip symbols panic abort",
    "safety comment unsafe block",
    "audit surface minimize unsafe",
    "trust boundary stride threat model",
    "tampering information disclosure denial of service",
    "elevation of privilege spoofing",
    "first call dim validation",
    "atomic bool swap relaxed",
    "arc dyn embedder",
    "owned clone cheap inner arc",
    "with embedder consumes self",
    "chain form fluent api",
    "kwarg bag form python idiom",
    "approach a python wrapper",
    "approach b napi extension",
    "codegen drift parity check",
    "surface toml annotation source",
    "abi stable wheel cross version",
    "n api version eight pin",
    "node twenty lts",
    "python three eleven plus",
    "maturin develop release wheel",
    "vitest spec mts convention",
    "pytest marker bindings it",
    "moon dev instance localhost",
    "redis port six three eight zero",
    "postgres five four three two",
    "cdylib link error workspace test",
    "scripts sdk real evidence",
    "ollama replay precompute embeddings",
    "lunaris bench harness baseline",
    "twenty five millisecond p fifty target",
    "ten thousand by one thousand squad",
    "recall at three eighty six percent",
    "ten point three ms p fifty strict",
    "twenty point eight ms p ninety nine",
    "longmemeval locomo er f one",
    "mem zero zep cognee differentiation",
    "helios first downstream consumer",
    "langgraph crewai letta second wave",
    "blueprint canonical decision source",
    "phase nineteen fastembed adoption",
    "phase twenty byo onnx execution provider",
    "phase twenty one sdk embedder config",
    "phase twenty two remote embedder follow up",
    "wave parallelism execution",
    "tdd red green refactor",
    "atomic commit individual plan",
    "summary md plan directory",
];

const CORPUS_LEN: usize = 102;
const EMBED_DIM: usize = 768;

// ---------------------------------------------------------------------------
// Test entry
// ---------------------------------------------------------------------------

/// Cross-SDK parity for `EmbedderConfig.fastembed()` defaults.
///
/// The two SDK fixture scripts produce 100×768 f32 matrices using the same
/// model + cache; this test asserts bit-exact equality. ONNX Runtime is
/// deterministic on CPU for fixed inputs, so any divergence here is a real
/// bug (tokenizer drift, FFI corruption, codegen-vs-handwritten divergence).
#[test]
fn sdk_embedder_parity_fastembed_default() {
    // Skip cleanly if a toolchain is missing — better signal than a panic
    // from `Command::status()` because the operator still gets the message.
    if !has_executable("python") && !has_executable("python3") {
        eprintln!("SKIP sdk_embedder_parity: python/python3 not on PATH");
        return;
    }
    if !has_executable("node") {
        eprintln!("SKIP sdk_embedder_parity: node not on PATH");
        return;
    }

    let tmp = tempdir("lunaris-sdk-parity");
    let corpus_path = tmp.join("corpus.txt");
    let py_out = tmp.join("py.bin");
    let ts_out = tmp.join("ts.bin");
    let cache_dir = tmp.join("fastembed-cache");
    fs::create_dir_all(&cache_dir).expect("create cache_dir");

    write_corpus(&corpus_path, PARITY_CORPUS).expect("write corpus");

    let py_script = fixture_path("embed_via_py.py");
    let ts_script = fixture_path("embed_via_ts.mjs");
    assert!(py_script.is_file(), "missing py fixture: {}", py_script.display());
    assert!(ts_script.is_file(), "missing ts fixture: {}", ts_script.display());

    run_py_fixture(&py_script, &corpus_path, &py_out, &cache_dir);
    run_ts_fixture(&ts_script, &corpus_path, &ts_out, &cache_dir);

    let py_mat = read_f32_matrix(&py_out);
    let ts_mat = read_f32_matrix(&ts_out);

    assert_eq!(
        py_mat.len(),
        ts_mat.len(),
        "vector count mismatch: py={} ts={}",
        py_mat.len(),
        ts_mat.len()
    );
    assert_eq!(py_mat.len(), CORPUS_LEN, "expected {CORPUS_LEN} rows");

    for (i, (py_vec, ts_vec)) in py_mat.iter().zip(ts_mat.iter()).enumerate() {
        assert_eq!(py_vec.len(), EMBED_DIM, "py[{i}] dim={}", py_vec.len());
        assert_eq!(ts_vec.len(), EMBED_DIM, "ts[{i}] dim={}", ts_vec.len());
        for (j, (a, b)) in py_vec.iter().zip(ts_vec.iter()).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "parity mismatch at row {i} col {j}: input={input:?} py={a} ts={b}",
                input = PARITY_CORPUS[i]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

fn tempdir(prefix: &str) -> PathBuf {
    let base = std::env::temp_dir();
    let unique = format!(
        "{}-{}-{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let dir = base.join(unique);
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn write_corpus(path: &Path, corpus: &[&str]) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    for line in corpus {
        // NOTE: corpus strings are hand-written and contain no newlines —
        // one input per line is a safe contract for the fixtures.
        assert!(!line.contains('\n'), "corpus entry must not contain newline");
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

fn has_executable(bin: &str) -> bool {
    // POSIX-ish path probe — good enough for the test gate (the cargo
    // feature is the primary opt-in; this just protects against a partial
    // setup).
    if let Ok(path) = std::env::var("PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path.split(sep) {
            let candidate = Path::new(dir).join(bin);
            if candidate.is_file() {
                return true;
            }
            #[cfg(windows)]
            {
                let with_exe = Path::new(dir).join(format!("{bin}.exe"));
                if with_exe.is_file() {
                    return true;
                }
            }
        }
    }
    false
}

fn python_bin() -> &'static str {
    if has_executable("python3") { "python3" } else { "python" }
}

fn run_py_fixture(script: &Path, corpus: &Path, out: &Path, cache_dir: &Path) {
    let status = Command::new(python_bin())
        .arg(script)
        .arg("--corpus")
        .arg(corpus)
        .arg("--out")
        .arg(out)
        .arg("--cache-dir")
        .arg(cache_dir)
        .status()
        .expect("spawn python fixture");
    assert!(
        status.success(),
        "py fixture exited with {status:?}; ensure `maturin develop` has been run in crates/lunaris-py"
    );
}

fn run_ts_fixture(script: &Path, corpus: &Path, out: &Path, cache_dir: &Path) {
    let status = Command::new("node")
        .arg(script)
        .arg("--corpus")
        .arg(corpus)
        .arg("--out")
        .arg(out)
        .arg("--cache-dir")
        .arg(cache_dir)
        .status()
        .expect("spawn node fixture");
    assert!(
        status.success(),
        "ts fixture exited with {status:?}; ensure `npm run build` has been run in crates/lunaris-ts"
    );
}

/// Decode `[u32_le count][u32_le dim][count*dim*4 bytes f32_le]` into a 2-d matrix.
fn read_f32_matrix(path: &Path) -> Vec<Vec<f32>> {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read matrix {}: {e}", path.display()));
    assert!(bytes.len() >= 8, "matrix file too short: {} bytes", bytes.len());
    let count = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let dim = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    assert_eq!(count, CORPUS_LEN, "header count={count} != expected {CORPUS_LEN}");
    assert_eq!(dim, EMBED_DIM, "header dim={dim} != expected {EMBED_DIM}");
    let expected_len = 8 + count * dim * 4;
    assert_eq!(
        bytes.len(),
        expected_len,
        "body length {} != header-implied {expected_len}",
        bytes.len()
    );
    let mut out = Vec::with_capacity(count);
    let mut cursor = 8;
    for _ in 0..count {
        let mut row = Vec::with_capacity(dim);
        for _ in 0..dim {
            let f = f32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
            row.push(f);
            cursor += 4;
        }
        out.push(row);
    }
    out
}
