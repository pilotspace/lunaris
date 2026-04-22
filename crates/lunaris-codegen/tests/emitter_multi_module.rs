//! Plan 11-02a Task 2 end-to-end emitter test.
//!
//! Exercises the new IR + emitter machinery (`IrTyRef::Handle`,
//! `IrModule::rust_crate_path`, `IrMethod::clone_self`, per-module
//! `register_generated_{suffix}` emission, async-owned + clone_self) on a
//! synthetic `SurfaceIR` with TWO modules — one default (`::lunaris`),
//! one with an explicit `rust_crate_path`. No `annotations/surface.toml`
//! is touched; the current committed `generated_*.rs` / `.d.ts`
//! snapshots stay bit-stable through Plan 11-02a (see `surface_snapshot`
//! and the parity-check CI).
//!
//! Assertions mirror the `must_haves.artifacts.contains` fragments in
//! the plan frontmatter so a regression on the emitter shape is caught
//! by this test rather than discovered by Plan 11-02b's host-wiring
//! compile failure.

use lunaris_codegen::{
    IrAsync, IrKind, IrMethod, IrModule, IrParam, IrReceiver, IrReturn, IrTyRef, IrType,
    SCHEMA_VERSION, SurfaceIR, emit_py, emit_ts,
};

fn method(
    name: &str,
    is_async: IrAsync,
    receiver: IrReceiver,
    params: Vec<IrParam>,
    returns: IrReturn,
    clone_self: bool,
) -> IrMethod {
    IrMethod {
        name: name.into(),
        is_async,
        receiver,
        params,
        returns,
        clone_self,
        doc: None,
    }
}

fn ty(name: &str, kind: IrKind, methods: Vec<IrMethod>) -> IrType {
    IrType {
        name: name.into(),
        kind,
        source_file: None,
        doc: None,
        methods,
    }
}

fn multi_module_ir() -> SurfaceIR {
    // Module A — legacy-shape `lunaris` module with no `rust_crate_path`.
    // One opaque type with a trivial static constructor + ref_self recall.
    let lunaris = IrModule {
        name: "lunaris".into(),
        rust_crate_path: None,
        types: vec![ty(
            "Lunaris",
            IrKind::Opaque,
            vec![method(
                "open",
                IrAsync::Yes,
                IrReceiver::None,
                vec![IrParam { name: "url".into(), ty: IrTyRef::Str }],
                IrReturn { ty: IrTyRef::RefSelf, fallible: true },
                false,
            )],
        )],
    };

    // Module B — synthetic fixture crate with explicit rust_crate_path.
    // Type1 — opaque wrapper with Handle-accepting constructor and an
    // owned-self clone_self method returning RefSelf (D3 + D6 paths).
    // Type2 — opaque wrapper with an async-owned-self clone_self method
    // returning Vec<Hit> (async-owned arm + rust_owned_ty allow-list).
    let fixture = IrModule {
        name: "fixture_crate.sub".into(),
        rust_crate_path: Some("::fixture_crate::sub".into()),
        types: vec![
            ty(
                "Type1",
                IrKind::Builder,
                vec![
                    method(
                        "new",
                        IrAsync::No,
                        IrReceiver::None,
                        vec![IrParam {
                            name: "lunaris".into(),
                            ty: IrTyRef::Handle { name: "Lunaris".into() },
                        }],
                        IrReturn { ty: IrTyRef::RefSelf, fallible: false },
                        false,
                    ),
                    method(
                        "with_flag",
                        IrAsync::No,
                        IrReceiver::Owned,
                        vec![IrParam { name: "flag".into(), ty: IrTyRef::Bool }],
                        IrReturn { ty: IrTyRef::RefSelf, fallible: false },
                        /* clone_self */ true,
                    ),
                ],
            ),
            ty(
                "Type2",
                IrKind::Opaque,
                vec![method(
                    "search",
                    IrAsync::Yes,
                    IrReceiver::Owned,
                    vec![IrParam { name: "query".into(), ty: IrTyRef::Str }],
                    IrReturn {
                        ty: IrTyRef::Vec {
                            inner: Box::new(IrTyRef::Named { name: "Hit".into() }),
                        },
                        fallible: true,
                    },
                    /* clone_self */ true,
                )],
            ),
        ],
    };

    SurfaceIR { schema_version: SCHEMA_VERSION, modules: vec![lunaris, fixture] }
}

#[test]
fn py_emits_per_module_register_fns() {
    let ir = multi_module_ir();
    let py = emit_py(&ir);
    assert!(
        py.contains("pub(crate) fn register_generated_lunaris("),
        "missing register_generated_lunaris in emitted Py — got:\n{py}"
    );
    assert!(
        py.contains("pub(crate) fn register_generated_fixture_crate_sub("),
        "missing register_generated_fixture_crate_sub — got:\n{py}"
    );
    // Legacy `register_generated(` fn MUST NOT appear when multiple
    // modules are present (the shim is Plan 11-02b's concern).
    assert!(
        !py.contains("pub(crate) fn register_generated("),
        "legacy register_generated fn leaked into multi-module emit"
    );
}

#[test]
fn py_call_sites_use_module_specific_rust_crate_path() {
    let ir = multi_module_ir();
    let py = emit_py(&ir);
    assert!(
        py.contains("::lunaris::Lunaris::open("),
        "module-A call-site should use ::lunaris:: prefix — got:\n{py}"
    );
    assert!(
        py.contains("::fixture_crate::sub::Type1::new("),
        "module-B call-site should use ::fixture_crate::sub prefix — got:\n{py}"
    );
}

#[test]
fn py_handle_variant_lowers_to_arc_clone() {
    let ir = multi_module_ir();
    let py = emit_py(&ir);
    // Type1::new takes a Handle { name = "Lunaris" } — emitted should bind
    // the owned Arc via `.inner.clone()`, not pythonize::depythonize.
    assert!(
        py.contains("let lunaris_owned: ::std::sync::Arc<::lunaris::Lunaris> = lunaris.inner.clone();"),
        "Handle param did not lower to Arc::clone — got:\n{py}"
    );
    // Handle variant param type is `&PyLunaris`, not `&Bound<'_, PyAny>`.
    assert!(
        py.contains("lunaris: &PyLunaris"),
        "Handle param type should be &PyLunaris — got:\n{py}"
    );
}

#[test]
fn py_clone_self_owned_sync_body() {
    let ir = multi_module_ir();
    let py = emit_py(&ir);
    // Type1::with_flag is (IrAsync::No, Owned, clone_self=true, RefSelf)
    assert!(
        py.contains("let out = self.inner.as_ref().clone().with_flag(flag);"),
        "clone_self body missing for Type1::with_flag — got:\n{py}"
    );
    assert!(
        py.contains("Ok(PyType1 { inner: Arc::new(out) })"),
        "clone_self return wrap missing — got:\n{py}"
    );
}

#[test]
fn py_async_owned_clone_self_uses_future_into_py() {
    let ir = multi_module_ir();
    let py = emit_py(&ir);
    // Type2::search is (IrAsync::Yes, Owned, clone_self=true, Vec<Hit>)
    assert!(
        py.contains("let cloned = self.inner.as_ref().clone();"),
        "async-owned clone_self should clone inner BEFORE future_into_py — got:\n{py}"
    );
    assert!(
        py.contains("future_into_py(py, async move {"),
        "async-owned clone_self must wrap .await in future_into_py — got:\n{py}"
    );
    assert!(
        py.contains("cloned.search(&query).await.map_err(py_err)?;"),
        "async-owned call-site missing — got:\n{py}"
    );
}

#[test]
fn py_preserves_gil_invariant_on_new_arms() {
    // Defence-in-depth: reuse the shape-test's scan logic inline to make
    // sure our new async-owned arm keeps every `.await` inside
    // `future_into_py(py, async move {`. This test narrows the scope to
    // the multi-module IR — the main `emitter_shape::py_emits_future_into_py_for_every_await`
    // test still covers the real committed surface.
    let ir = multi_module_ir();
    let py = emit_py(&ir);
    let mut inside = false;
    let mut brace_at_open = 0i32;
    let mut depth = 0i32;
    let mut offending: Vec<(usize, String)> = vec![];
    for (i, line) in py.lines().enumerate() {
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        if !inside && line.contains("future_into_py(py, async move") {
            inside = true;
            brace_at_open = depth;
        }
        depth += opens - closes;
        if inside && depth <= brace_at_open {
            inside = false;
        }
        if line.contains(".await") && !inside {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with("/*") {
                continue;
            }
            offending.push((i + 1, line.to_string()));
        }
    }
    assert!(
        offending.is_empty(),
        "emit_py bare `.await` outside future_into_py in multi-module emit at: {offending:?}"
    );
}

#[test]
fn ts_emits_per_module_structs_with_rust_crate_path() {
    let ir = multi_module_ir();
    let out = emit_ts(&ir);
    let rust = out.rust_glue;
    // Module A — legacy path.
    assert!(
        rust.contains("Arc<::lunaris::Lunaris>"),
        "module-A struct did not use ::lunaris path — got:\n{rust}"
    );
    assert!(
        rust.contains("::lunaris::Lunaris::open("),
        "module-A call-site did not use ::lunaris prefix — got:\n{rust}"
    );
    // Module B — fixture path.
    assert!(
        rust.contains("Arc<::fixture_crate::sub::Type1>"),
        "module-B struct did not use ::fixture_crate::sub path — got:\n{rust}"
    );
    assert!(
        rust.contains("Arc<::fixture_crate::sub::Type2>"),
        "module-B Type2 struct did not use ::fixture_crate::sub path — got:\n{rust}"
    );
}

#[test]
fn ts_handle_variant_lowers_to_arc_clone() {
    let ir = multi_module_ir();
    let out = emit_ts(&ir);
    let rust = out.rust_glue;
    assert!(
        rust.contains("let lunaris_owned: ::std::sync::Arc<::lunaris::Lunaris> = lunaris.inner.clone();"),
        "TS Handle param did not lower to Arc::clone — got:\n{rust}"
    );
    assert!(
        rust.contains("lunaris: &Lunaris"),
        "TS Handle param type should be &Lunaris — got:\n{rust}"
    );
    // .d.ts carries the TS class name (no `Py` prefix).
    let dts = out.dts;
    assert!(
        dts.contains("new(lunaris: Lunaris)"),
        "TS .d.ts should carry Handle param as class name — got:\n{dts}"
    );
}

#[test]
fn ts_clone_self_owned_sync_body() {
    let ir = multi_module_ir();
    let out = emit_ts(&ir);
    let rust = out.rust_glue;
    assert!(
        rust.contains("let out = self.inner.as_ref().clone().with_flag(flag);"),
        "TS clone_self body missing for Type1::with_flag — got:\n{rust}"
    );
    assert!(
        rust.contains("Ok(Type1 { inner: Arc::new(out) })"),
        "TS clone_self return wrap missing — got:\n{rust}"
    );
}

/// Plan 11-02b Rule 1 extension — `rust_owned_ty` / `rust_owned_ty_ts`
/// allow-list carries concrete Rust spellings for Phase 10/11 wrapper
/// param shapes that can't be expressed as-is through `kind = "json"`
/// (the planner's D4(b) claim was empirically wrong — see emit_py.rs
/// commentary). These new names — `DocumentChunks`, `UnixMs`, `StrList`,
/// `JsonValue` — ride through the existing `Named` variant with no IR
/// grammar change.
#[test]
fn py_and_ts_new_allow_list_entries_lower_to_concrete_rust_spellings() {
    use lunaris_codegen::{
        IrAsync, IrKind, IrMethod, IrModule, IrParam, IrReceiver, IrReturn, IrTyRef, IrType,
        SCHEMA_VERSION, SurfaceIR, emit_py, emit_ts,
    };

    let fixture = IrModule {
        name: "fixture.allow_list".into(),
        rust_crate_path: Some("::fixture::allow_list".into()),
        types: vec![IrType {
            name: "Probe".into(),
            kind: IrKind::Opaque,
            source_file: None,
            doc: None,
            methods: vec![
                IrMethod {
                    name: "ingest".into(),
                    is_async: IrAsync::Yes,
                    receiver: IrReceiver::RefSelf,
                    params: vec![IrParam {
                        name: "chunks".into(),
                        ty: IrTyRef::Named { name: "DocumentChunks".into() },
                    }],
                    returns: IrReturn { ty: IrTyRef::Unit, fallible: true },
                    clone_self: false,
                    doc: None,
                },
                IrMethod {
                    name: "ingest_commit".into(),
                    is_async: IrAsync::Yes,
                    receiver: IrReceiver::RefSelf,
                    params: vec![IrParam {
                        name: "committer_date_unix_ms".into(),
                        ty: IrTyRef::Named { name: "UnixMs".into() },
                    }],
                    returns: IrReturn { ty: IrTyRef::Unit, fallible: true },
                    clone_self: false,
                    doc: None,
                },
                IrMethod {
                    name: "attendees".into(),
                    is_async: IrAsync::Yes,
                    receiver: IrReceiver::RefSelf,
                    params: vec![IrParam {
                        name: "ids".into(),
                        ty: IrTyRef::Named { name: "StrList".into() },
                    }],
                    returns: IrReturn { ty: IrTyRef::Unit, fallible: true },
                    clone_self: false,
                    doc: None,
                },
                IrMethod {
                    name: "filter".into(),
                    is_async: IrAsync::No,
                    receiver: IrReceiver::Owned,
                    params: vec![IrParam {
                        name: "value".into(),
                        ty: IrTyRef::Named { name: "JsonValue".into() },
                    }],
                    returns: IrReturn { ty: IrTyRef::RefSelf, fallible: false },
                    clone_self: true,
                    doc: None,
                },
            ],
        }],
    };
    let ir = SurfaceIR { schema_version: SCHEMA_VERSION, modules: vec![fixture] };

    let py = emit_py(&ir);
    assert!(
        py.contains("let chunks_owned: Vec<(String, serde_json::Map<String, serde_json::Value>)> ="),
        "DocumentChunks did not lower to concrete tuple-of-map spelling in Py — got:\n{py}"
    );
    assert!(
        py.contains("let committer_date_unix_ms_owned: i64 ="),
        "UnixMs did not lower to i64 in Py — got:\n{py}"
    );
    assert!(
        py.contains("let ids_owned: Vec<String> ="),
        "StrList did not lower to Vec<String> in Py — got:\n{py}"
    );
    assert!(
        py.contains("let value_owned: serde_json::Value ="),
        "JsonValue did not lower to serde_json::Value in Py — got:\n{py}"
    );

    let ts = emit_ts(&ir).rust_glue;
    assert!(
        ts.contains("let chunks_owned: Vec<(String, serde_json::Map<String, serde_json::Value>)> ="),
        "DocumentChunks did not lower to concrete spelling in TS — got:\n{ts}"
    );
    assert!(
        ts.contains("let committer_date_unix_ms_owned: i64 ="),
        "UnixMs did not lower to i64 in TS — got:\n{ts}"
    );
    assert!(
        ts.contains("let ids_owned: Vec<String> ="),
        "StrList did not lower to Vec<String> in TS — got:\n{ts}"
    );
    assert!(
        ts.contains("let value_owned: serde_json::Value ="),
        "JsonValue did not lower to serde_json::Value in TS — got:\n{ts}"
    );
}

#[test]
fn ts_async_owned_clone_self() {
    let ir = multi_module_ir();
    let out = emit_ts(&ir);
    let rust = out.rust_glue;
    // Plan 11-02b — async-owned clone_self branches on return type: when
    // the return is Vec<Named> (not RefSelf/Named), the sig is
    // `napi::Result<Vec<serde_json::Value>>` (JSON pass-through, same as
    // the async-ref_self arm). Wrapping Vec<Hit> into `Type2` would be a
    // bug (Type2 is the source type, not a Vec container).
    assert!(
        rust.contains("pub async fn search(&self, query: String) -> napi::Result<Vec<serde_json::Value>>"),
        "async-owned clone_self signature wrong (expected Vec<Value>) — got:\n{rust}"
    );
    assert!(
        rust.contains("let cloned = self.inner.as_ref().clone();"),
        "async-owned clone_self should clone inner — got:\n{rust}"
    );
    assert!(
        rust.contains("cloned.search(&query).await.map_err(napi_err)?;"),
        "async-owned call-site missing — got:\n{rust}"
    );
}
