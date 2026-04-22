//! Plan 08-02 — Python↔Rust conversion helpers.
//!
//! The codegen-emitted wrappers in `generated.rs` already use
//! `pythonize::depythonize` for `kind = "json"` parameters (see
//! `emit_py.rs::format_call_args`). That covers `Lunaris::ingest(episode)`
//! directly because [`::lunaris::Episode`] derives `Serialize + Deserialize`
//! (see `crates/lunaris-core/src/primitives.rs:16`).
//!
//! `Lunaris::forget(req)` is the odd one out: [`::lunaris::forget::ForgetRequest`]
//! derives only `Clone + Debug + PartialEq + Eq` (see
//! `crates/lunaris/src/forget.rs:126-130`) — NOT `Deserialize`. The raw
//! codegen `pythonize::depythonize::<ForgetRequest>` call therefore fails to
//! compile. This module exposes [`forget_req_from_py`] which manually
//! assembles a `ForgetRequest` from its Deserialize-able parts
//! (`ForgetTarget` + `ForgetOptions`).
//!
//! `Hit` serialisation back to Python flows through `pythonize::pythonize`
//! inside `dsl.rs::execute` and is not duplicated here.
//!
//! Plan 08-02 T-08-02-01 mitigation: all depythonize sites translate serde
//! errors through `py_err_str("VALIDATE", ...)` so malformed payloads surface
//! as `lunaris.LunarisError(code="VALIDATE")` on the Python side.

use pyo3::prelude::*;
use pyo3::types::PyDict;

use ::lunaris::forget::{ForgetOptions, ForgetRequest, ForgetTarget};

/// Convert a Python dict of shape `{"target": {...}, "options": {...}}` into
/// a typed [`ForgetRequest`]. `options` is optional — omitting it yields the
/// soft-delete default (`ForgetOptions::default()`).
///
/// NOTE — currently unused: the codegen-emitted `PyLunaris::forget` wrapper
/// uses the raw `pythonize::depythonize::<ForgetRequest>` path now that
/// `ForgetRequest` derives `Deserialize` (Plan 08-02 additive upstream).
/// Kept as a reserved seam in case a future plan wants to accept richer
/// Python dict shapes (e.g. {"scope": "x"} shorthand) without going through
/// the serde schema.
#[allow(dead_code)]
pub(crate) fn forget_req_from_py(obj: &Bound<'_, PyAny>) -> PyResult<ForgetRequest> {
    let dict: &Bound<'_, PyDict> = obj.downcast::<PyDict>().map_err(|_| {
        crate::errors::py_err_str(
            "VALIDATE",
            "forget request must be a dict with keys 'target' and optional 'options'",
        )
    })?;

    let target_any = dict
        .get_item("target")
        .map_err(|e| crate::errors::py_err_str("VALIDATE", format!("target lookup: {e}")))?
        .ok_or_else(|| {
            crate::errors::py_err_str("VALIDATE", "forget request missing required key 'target'")
        })?;

    let target: ForgetTarget = pythonize::depythonize(&target_any).map_err(|e| {
        crate::errors::py_err_str("VALIDATE", format!("forget target deserialize: {e}"))
    })?;

    let options = if let Some(opt_any) = dict
        .get_item("options")
        .map_err(|e| crate::errors::py_err_str("VALIDATE", format!("options lookup: {e}")))?
    {
        let parsed: ForgetOptions = pythonize::depythonize(&opt_any).map_err(|e| {
            crate::errors::py_err_str("VALIDATE", format!("forget options deserialize: {e}"))
        })?;
        parsed
    } else {
        ForgetOptions::default()
    };

    Ok(ForgetRequest { target, options })
}
