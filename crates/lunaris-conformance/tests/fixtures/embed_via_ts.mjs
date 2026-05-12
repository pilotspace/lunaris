// Plan 21-03 Task 1 — TypeScript-side fixture for cross-SDK embedder parity.
//
// Reads `--corpus` (one input per line, UTF-8), builds
// `EmbedderConfig.fastembed({ cacheDir })` from the `lunaris-ts` napi
// bindings, runs the corpus through the inner embedder, and writes the
// resulting `count × dim` f32 matrix to `--out` using the shared layout:
//
//     [u32 count_le][u32 dim_le][count * dim * 4 bytes f32_le row-major]
//
// Usage:
//   node embed_via_ts.mjs --corpus <path> --out <path> --cache-dir <path>
//
// Prerequisites:
//   cd crates/lunaris-ts && npm install && npm run build
//
// The fastembed default downloads ~600 MB of ONNX weights into
// `--cache-dir` on first run; subsequent runs reuse the cache.
//
// ## Module resolution
//
// Imports the lunaris-ts artifact via a relative path computed from
// `import.meta.url` (../../../lunaris-ts/index.js). This is deterministic
// across cwd changes and does NOT require `node_modules/lunaris` to be
// linked — the only prerequisite is that `npm run build` produced
// `index.js` + the native `.node` binary in `crates/lunaris-ts/`.
//
// ## Embedder probe contract
//
// Same shape as the Python sibling: this fixture looks for an
// `embedderConfigEmbedBatch(cfg, inputs)` free function on the lunaris-ts
// module. That probe is a `#[cfg(feature = "bindings-it")]` napi export
// the Plan 21-03 follow-up landed alongside the parity scaffold; release
// builds strip it.
//
// If the probe is missing the fixture exits with code 75 (EX_TEMPFAIL-style)
// and a descriptive stderr message. The Rust orchestrator treats this as
// a test failure with the hint "rebuild lunaris-ts with --features
// bindings-it" so the failure mode is informative.

import { writeFile, readFile, mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { parseArgs } from "node:util";

const HERE = dirname(fileURLToPath(import.meta.url));
// Path layout:
//   crates/lunaris-conformance/tests/fixtures/embed_via_ts.mjs
//   crates/lunaris-ts/index.js
const LUNARIS_TS_INDEX = resolve(HERE, "../../../lunaris-ts/index.js");

const { values } = parseArgs({
  options: {
    corpus: { type: "string" },
    out: { type: "string" },
    "cache-dir": { type: "string" },
  },
});

for (const key of ["corpus", "out", "cache-dir"]) {
  if (!values[key]) {
    console.error(`embed_via_ts: missing required --${key}`);
    process.exit(2);
  }
}

const corpusPath = values.corpus;
const outPath = values.out;
const cacheDir = values["cache-dir"];

let mod;
try {
  mod = await import(LUNARIS_TS_INDEX);
} catch (err) {
  console.error(
    `embed_via_ts: failed to import lunaris-ts at ${LUNARIS_TS_INDEX}\n` +
      `  Ensure 'npm install && npm run build' has run in crates/lunaris-ts.\n` +
      `  Underlying error: ${err && err.message ? err.message : err}`,
  );
  process.exit(2);
}

const { EmbedderConfig, embedderConfigEmbedBatch } = mod;
if (typeof embedderConfigEmbedBatch !== "function") {
  console.error(
    "embed_via_ts: `embedderConfigEmbedBatch` probe is not exposed in this build.\n" +
      "  Rebuild with the bindings-it feature:\n" +
      "    cd crates/lunaris-ts && npm run build -- --features bindings-it",
  );
  process.exit(75); // EX_TEMPFAIL — Rust orchestrator surfaces actionable hint.
}

await mkdir(cacheDir, { recursive: true });

const raw = await readFile(corpusPath, "utf8");
const corpus = raw.split("\n").filter((line) => line.length > 0);
if (corpus.length === 0) {
  console.error("embed_via_ts: corpus is empty");
  process.exit(2);
}

console.error(
  `embed_via_ts: corpus=${corpus.length} cache_dir=${cacheDir} index=${LUNARIS_TS_INDEX}`,
);

const cfg = EmbedderConfig.fastembed({ cacheDir });
const matrix = await embedderConfigEmbedBatch(cfg, corpus);
if (matrix.length !== corpus.length) {
  console.error(
    `embed_via_ts: row count mismatch — corpus=${corpus.length} matrix=${matrix.length}`,
  );
  process.exit(3);
}

const count = matrix.length;
const dim = matrix[0]?.length ?? 0;
const buf = Buffer.alloc(8 + count * dim * 4);
buf.writeUInt32LE(count, 0);
buf.writeUInt32LE(dim, 4);
let offset = 8;
for (let i = 0; i < count; i += 1) {
  const row = matrix[i];
  if (row.length !== dim) {
    console.error(
      `embed_via_ts: ragged matrix — row ${i} dim=${row.length} != ${dim}`,
    );
    process.exit(3);
  }
  for (let j = 0; j < dim; j += 1) {
    buf.writeFloatLE(row[j], offset);
    offset += 4;
  }
}
await writeFile(outPath, buf);
