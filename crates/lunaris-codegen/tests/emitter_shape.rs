//! Shape tests for the two emitters.
//!
//! These tests lock:
//! 1. **T-08-01-03 (GIL discipline)**: the Python emitter places every
//!    `.await` inside a `pyo3_async_runtimes::tokio::future_into_py`
//!    closure. No bare `.await` may leak into generated Python code.
//! 2. **napi-rs shape**: the TypeScript emitter decorates every struct and
//!    method with `#[napi]` / `#[napi(factory)]`.
//! 3. **`--check` drift detection**: mutating a committed snapshot causes
//!    `check_surface` to report drift.
//! 4. **BLOCKER 3+4 carveout**: the walker lists exactly the three
//!    codegen-managed paths; a `conformance.rs` file placed anywhere in
//!    the workspace is invisible to parity-check by construction.

use lunaris_codegen::{check_surface, codegen_managed_paths, emit_py, emit_ts, extract_surface};
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

#[test]
fn py_emits_future_into_py_for_every_await() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let py = emit_py(&ir);
    // Every `.await` on a line must share the function body (i.e. be
    // preceded within the same `fn` item) by a `future_into_py(py, async
    // move {` opener. We implement this as a forward scan per line:
    let mut inside_future_into_py = false;
    let mut brace_depth_at_open = 0i32;
    let mut brace_depth = 0i32;
    let mut offending_lines: Vec<(usize, String)> = vec![];
    for (i, line) in py.lines().enumerate() {
        // Count braces in this line to track nesting.
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        if !inside_future_into_py && line.contains("future_into_py(py, async move") {
            inside_future_into_py = true;
            brace_depth_at_open = brace_depth;
        }
        brace_depth += opens - closes;
        if inside_future_into_py && brace_depth <= brace_depth_at_open {
            // Closing brace of the `async move` block reached.
            inside_future_into_py = false;
        }
        if line.contains(".await") && !inside_future_into_py {
            // Comments are allowed to mention .await.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                continue;
            }
            offending_lines.push((i + 1, line.to_string()));
        }
    }
    assert!(
        offending_lines.is_empty(),
        "emit_py produced bare `.await` outside future_into_py at: {offending_lines:?}"
    );
}

#[test]
fn ts_emits_napi_macros() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let out = emit_ts(&ir);
    let rust = out.rust_glue;
    let dts = out.dts;
    // Every `pub struct X` is preceded by `#[napi]` within two lines.
    for (i, line) in rust.lines().enumerate() {
        if line.starts_with("pub struct ") {
            let window = rust.lines().skip(i.saturating_sub(2)).take(3);
            let has_napi = window.clone().any(|l| l.trim() == "#[napi]");
            assert!(
                has_napi,
                "emit_ts produced a `pub struct` at line {} without #[napi]: {line:?}",
                i + 1
            );
        }
    }
    // The .d.ts declares `export class` for every opaque/builder type in
    // the IR.
    for module in &ir.modules {
        for ty in &module.types {
            let expected = format!("export class {}", ty.name);
            assert!(
                dts.contains(&expected),
                "emit_ts .d.ts missing '{expected}' — full dts=\n{dts}"
            );
        }
    }
    // Async `snapshot` → Promise<string> (Lsn is serialised as a decimal
    // string across the FFI boundary).
    assert!(
        dts.contains("snapshot(): Promise<string>"),
        "expected 'snapshot(): Promise<string>' in .d.ts"
    );
}

/// Test 3: `--check` reports drift when a snapshot is mutated.
///
/// We drive `check_surface` against a temp workspace root that shadows the
/// real `crates/lunaris-codegen/snapshots/` layout with a deliberately
/// drifted `generated_py.rs`. The real repo is not touched.
#[test]
fn check_mode_detects_drift() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let tmp = tempfile::tempdir().expect("tempdir");
    let snapshots = tmp.path().join("crates/lunaris-codegen/snapshots");
    std::fs::create_dir_all(&snapshots).unwrap();

    // Write a deliberately drifted generated_py.rs; the other two files are
    // absent so the walker also reports drift for them.
    std::fs::write(
        snapshots.join("generated_py.rs"),
        "// intentionally drifted content — parity-check MUST notice.",
    )
    .unwrap();

    let outcome = check_surface(tmp.path(), &ir).expect("check_surface");
    assert!(!outcome.is_clean(), "expected drift to be detected — got clean outcome");
    // The three codegen-managed paths are all drifted (one because of
    // explicit mutation; two because they don't exist in the temp tree).
    assert_eq!(outcome.drift.len(), 3);
}

/// Test 4: the walker skips `conformance.rs` by construction.
///
/// BLOCKER 3+4 / T-08-01-05 resolution. We place a file at
/// `crates/lunaris-py/src/conformance.rs` inside the temp workspace root
/// (and also at `crates/lunaris-ts/src/conformance.rs`) plus exact copies
/// of the three codegen-managed snapshots. The walker MUST report a clean
/// tree: it never enumerates `conformance.rs` files because
/// [`codegen_managed_paths`] returns an explicit three-path list, not a
/// recursive scan.
#[test]
fn check_mode_skips_conformance_rs() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let tmp = tempfile::tempdir().expect("tempdir");

    // Copy the real committed snapshots into the temp tree so the walker
    // sees no drift.
    for managed in codegen_managed_paths(&ir) {
        let dest = tmp.path().join(&managed.relative);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, &managed.expected).unwrap();
    }

    // Drop conformance.rs files in the future host-crate src dirs with
    // arbitrary (drifted) content. The walker MUST ignore them.
    let py_conf = tmp.path().join("crates/lunaris-py/src/conformance.rs");
    let ts_conf = tmp.path().join("crates/lunaris-ts/src/conformance.rs");
    std::fs::create_dir_all(py_conf.parent().unwrap()).unwrap();
    std::fs::create_dir_all(ts_conf.parent().unwrap()).unwrap();
    std::fs::write(
        &py_conf,
        "// NOT codegen-managed — conformance-only helpers (Plan 08-04).\n// arbitrary handwritten content here.",
    )
    .unwrap();
    std::fs::write(
        &ts_conf,
        "// NOT codegen-managed — conformance-only helpers (Plan 08-04).\n// different arbitrary handwritten content.",
    )
    .unwrap();

    let outcome = check_surface(tmp.path(), &ir).expect("check_surface");
    assert!(
        outcome.is_clean(),
        "parity-check walker leaked into conformance.rs — should be ignored by construction; drift={:#?}",
        outcome.drift
    );
}

#[test]
fn codegen_managed_paths_enumerates_exactly_three() {
    // Defense-in-depth against a future refactor that turns the walker
    // recursive: any such change must update `codegen_managed_paths` to
    // preserve the conformance.rs carveout explicitly.
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let paths = codegen_managed_paths(&ir);
    assert_eq!(paths.len(), 3, "expected exactly 3 managed paths, got {}", paths.len());
    let names: Vec<_> = paths.iter().map(|p| p.relative.display().to_string()).collect();
    assert!(names.iter().any(|n| n.contains("generated_py.rs")));
    assert!(names.iter().any(|n| n.contains("generated_ts.rs")));
    assert!(names.iter().any(|n| n.contains("generated_ts.d.ts")));
    assert!(
        !names.iter().any(|n| n.contains("conformance.rs")),
        "managed-path list must never contain conformance.rs"
    );
}

/// WR-01 lock (Phase 8 code review):
/// The PyO3 emitter MUST NOT produce `unimplemented!(...)` in the
/// owned-self builder branch. Rust panics cross the FFI boundary as
/// `PanicException` (a `BaseException` subclass) which Python
/// `except RuntimeError:` / `except LunarisError:` cannot catch.
/// Builder stubs must return `Err(PyNotImplementedError::new_err(...))`
/// so callers see a normal `NotImplementedError` they can handle.
#[test]
fn emit_py_unimplemented_returns_py_err() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let py = emit_py(&ir);
    assert!(
        !py.contains("unimplemented!(\"Plan 08-02 wires the body\")"),
        "emit_py still emits `unimplemented!(...)` in an owned-self builder branch — must return `Err(PyNotImplementedError::new_err(...))` (WR-01)"
    );
    assert!(
        py.contains("pyo3::exceptions::PyNotImplementedError::new_err("),
        "emit_py does not emit a `PyNotImplementedError::new_err(...)` anywhere — WR-01 fix missing"
    );
    // Defence-in-depth: no Rust `panic!(...)` escapes should be emitted
    // by the PyO3 emitter either.
    assert!(
        !py.contains("panic!("),
        "emit_py emits a `panic!(...)` — FFI-facing panics must be lifted into PyErr returns (WR-01)"
    );
}

/// WR-02 lock (Phase 8 code review):
/// The TS emitter's sync-factory path MUST NOT `.unwrap_or_else(|e| panic!(...))`
/// on serde failure. When any param is `Vec<T>` / `Named<T>` / `Option<T>` /
/// `Json`, the factory must return `napi::Result<Self>` and route the
/// serde error through `.map_err(napi_err)?` — mirror of the async
/// branch. `Graph::anchored` is the load-bearing caller (takes
/// `Vec<EntityId>`).
#[test]
fn emit_ts_graph_anchored_fallible() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let out = emit_ts(&ir);
    let rust = out.rust_glue;
    assert!(
        !rust.contains(".unwrap_or_else(|e| panic!"),
        "emit_ts still emits `.unwrap_or_else(|e| panic!(...))` — sync factories with serde-failing params must return `napi::Result<Self>` (WR-02)"
    );
    assert!(
        !rust.contains("panic!("),
        "emit_ts emits `panic!(...)` — FFI-facing panics must be lifted into napi::Result returns (WR-02)"
    );
    // Specifically require `Graph::anchored` to be fallible.
    assert!(
        rust.contains("pub fn anchored(entity_ids: Vec<serde_json::Value>, hops: u32) -> napi::Result<Self>"),
        "emit_ts did not produce a fallible `Graph::anchored` factory — expected `-> napi::Result<Self>` (WR-02)"
    );
}

#[test]
fn emit_is_deterministic() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let py1 = emit_py(&ir);
    let py2 = emit_py(&ir);
    assert_eq!(py1, py2, "emit_py is non-deterministic");
    let ts1 = emit_ts(&ir);
    let ts2 = emit_ts(&ir);
    assert_eq!(ts1, ts2, "emit_ts is non-deterministic");
}
