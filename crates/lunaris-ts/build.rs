// Plan 08-03 — napi-rs 3.x build script.
//
// `napi-build` emits the per-target linker flags needed to produce a
// Node-loadable `.node` cdylib (weak symbols on Linux, `-undefined
// dynamic_lookup` on macOS, DLL imports on Windows). Without this the
// produced shared object would fail to dlopen inside Node with
// "undefined symbol: napi_*" errors.

fn main() {
    napi_build::setup();
}
