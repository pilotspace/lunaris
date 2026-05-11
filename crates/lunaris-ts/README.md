# lunaris

napi-rs 3.x TypeScript bindings for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent memory engine.

## Installation

From source (local development):

```bash
cd crates/lunaris-ts
npm install
npm run build           # release build; produces lunaris.<triple>.node
npm test                # run vitest suite
```

A published npm package will ship with the v0.1.1 release via the Plan 08-04 multi-platform prebuild matrix.

For the full user-facing install + quickstart guide covering both TypeScript and Python, see [`docs/bindings.md`](../../docs/bindings.md).

## Requirements

- Node **20+** (NAPI ABI v8 pin — `abi_pin.spec.mts` asserts `process.versions.napi >= 8` at test startup).
- A Moon or Postgres backend reachable from the process; `moon://` and `postgres://` URL schemes are supported.

## Example

```typescript
import { open, Vector, Keyword } from "lunaris";

async function main() {
  const handle = await open("moon://127.0.0.1:6379");
  const lsn = await handle.ingest({
    id: "01JABCDEFGHJKMNPQRSTVWXYZ0",
    source: "ts-example",
    content: "Lunaris bi-temporal hello.",
    metadata: {},
    t_ref: null,
    bt: {
      valid: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
      sys: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
    },
  });
  console.log("ingested at", lsn);

  const hits = await handle.recall().execute();
  for (const h of hits) console.log(h);
}

main();
```

## Custom embedder + reranker

Pick a preset or bring your own ONNX model. The `EmbedderConfig` and
`RerankerConfig` factories swap the backend on a freshly-opened handle via
the chainable `withEmbedder` / `withReranker` extension; the env-driven
default remains in place for callers that don't chain.

```typescript
import { Lunaris, EmbedderConfig, RerankerConfig } from "lunaris";

const cfg = EmbedderConfig.fastembed({ cacheDir: "/var/cache/lunaris/fastembed" });
const handle = (await Lunaris.open("moon://127.0.0.1:6379"))
  .withEmbedder(cfg)
  .withReranker(RerankerConfig.noop());   // disable cross-encoder rerank
// ... ingest / recall as usual
```

See [`docs/sdk/embedder-config.md`](../../docs/sdk/embedder-config.md) for
the full customization guide — preset fastembed, preset Ollama, BYO ONNX
bytes, and BYO ONNX path — with troubleshooting and the FFI-cliff limits.

## Surface parity

The TypeScript class / method surface is generated from `crates/lunaris-codegen/annotations/surface.toml` (Plan 08-01). The parity-check CI job fails any PR that drifts the committed snapshot from the regenerated output — `npm i lunaris` never lags the Rust crate.

## Three-surface pipeline toggles

The `GraphPipeline` and `ConsolidatorPipeline` default to OFF (blueprint §5.1 / §5.2). Flip them at any of three surfaces:

| Surface | Example |
| ------- | ------- |
| Code    | `handle.graphPipeline.enable()` |
| Env     | `LUNARIS_GRAPH_ENABLED=1 node run.mjs` |
| Config  | `await open(url, { graphPipeline: { enabled: true } })` |

Resolution order: code > env > config — code is always authoritative.

## NAPI ABI pin

This crate pins the NAPI ABI to version **8** (Node 20 LTS stable ABI) via the `napi8` feature on the `napi` dep. Older Node runtimes (18.x) that ship NAPI 7 fail the `abi_pin.spec.mts` assertion at test startup with a readable reason — use Node 20+.
