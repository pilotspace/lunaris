import { defineConfig } from "vitest/config";

// Plan 08-03 — vitest configuration for the lunaris-ts binding tests.
//
// The tests exercise the compiled `.node` binary produced by
// `napi build --platform --release`. That binary is loaded on import via
// the `@napi-rs/cli`-generated `index.js` / `index.mjs` loader. Tests that
// need a live Moon or Postgres backend gate on `LUNARIS_MOON_URL`
// and skip when no reachable backend is detected — mirrors the Python
// `conftest.py::moon_backend_url` fixture shape.
export default defineConfig({
  test: {
    include: ["__test__/**/*.spec.mts"],
    // Tests that trigger async Rust work via napi-rs need a generous
    // timeout — the default 5 s is enough for offline paths but a cold
    // backend open + ingest can breach it on a slow dev box.
    testTimeout: 30000,
    // Run suites sequentially so tests that mutate process.env (the
    // env-surface toggle test) don't race against suites that read it.
    //
    // This was `poolOptions.threads.singleThread`, which Vitest 4 REMOVED —
    // and package.json pins vitest ^4.1.8. The option was being ignored, so
    // the sequential guarantee this comment describes was not in effect and
    // the env-mutation race it exists to prevent was live. A config key that
    // silently stops applying reads exactly like one that works.
    pool: "threads",
    fileParallelism: false,
  },
});
