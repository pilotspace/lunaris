# Lunaris SDK bindings — Python and TypeScript

Lunaris ships three first-party SDKs that talk to the same Rust memory
engine via the same annotated surface (`crates/lunaris-codegen/annotations/
surface.toml`):

| Language   | Package                       | Build tool          | Minimum runtime |
| ---------- | ----------------------------- | ------------------- | --------------- |
| Rust       | `lunaris` crate               | cargo               | Rust 1.94 MSRV  |
| Python     | `pip install lunaris`         | PyO3 0.26 + maturin | Python 3.11+    |
| TypeScript | `npm install @pilotspace/lunaris`         | napi-rs 3.x         | Node 20+        |

All three expose the same high-level handle surface:
`open / ingest / recall / forget / snapshot` — and the same composable
retrieve DSL (`Vector`, `Keyword`, `Graph`, `RetrievalBuilder`).
`cargo run -p lunaris-codegen -- --check` gates every PR so the three
surfaces never drift.

## Install

### Python

```bash
pip install lunaris
```

Prebuilt wheels ship for **5 targets** (BIND-PY-05):

- `linux-x86_64` (manylinux_2_28)
- `linux-aarch64` (manylinux_2_28)
- `macosx-x86_64`
- `macosx-arm64`
- `win-amd64`

The wheels use the `abi3-py311` stable ABI so **one wheel per target
covers Python 3.11, 3.12, and 3.13**.

If your architecture is missing a prebuilt wheel (see ROADMAP Phase 8
risk register row 2 — e.g. an aarch64 build that failed QEMU cross-
compile), fall back to a source install:

```bash
pip install lunaris --no-binary lunaris
```

This requires Rust 1.94+ and a working maturin toolchain. Source
installs take 2–5 minutes depending on the host.

### TypeScript

```bash
npm install @pilotspace/lunaris
```

Prebuilt `.node` binaries ship for **5 targets** (BIND-TS-05):

- `linux-x64`
- `linux-arm64`
- `darwin-x64`
- `darwin-arm64`
- `win32-x64`

The NAPI ABI is pinned to version **8** (Node 20 LTS) via the `napi8`
feature on the Rust `napi` dep. The `abi_pin.spec.mts` test asserts
`process.versions.napi >= 8` at test startup; older Node runtimes (18.x)
that ship NAPI 7 fail with a readable reason rather than a cryptic
`undefined symbol: napi_get_value_*` at dlopen time.

If your platform is missing a prebuilt binary, install from source:

```bash
npm install @pilotspace/lunaris --build-from-source
```

This requires Rust 1.94+ and the `@napi-rs/cli` toolchain.

## Quickstart

### Python

```python
import asyncio
import lunaris
import ulid


async def main():
    handle = await lunaris.open("moon://127.0.0.1:6380")

    lsn = await handle.ingest({
        "id": str(ulid.ULID()),
        "source": "py-quickstart",
        "content": "Lunaris bi-temporal memory — hello from Python.",
        "metadata": {},
        "t_ref": None,
        "bt": {
            "valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
            "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
        },
    })
    print("ingested at LSN", lsn)

    # Retrieve DSL — Vector / Keyword / Graph compose via .and_(), .fuse_rrf(),
    # .top(), .filter(), .as_of(). Terminal .execute() collapses the plan
    # into a single FFI call.
    hits = await (
        lunaris.RetrievalBuilder()
            .bind(handle)
            .top(5)
            .execute()
    )
    for h in hits:
        print(h)


asyncio.run(main())
```

### TypeScript

```typescript
import { open, RetrievalBuilder } from "@pilotspace/lunaris";

async function main() {
  const handle = await open("moon://127.0.0.1:6380");

  const lsn = await handle.ingest({
    id: "01JABCDEFGHJKMNPQRSTVWXYZ0", // 26-char Crockford-base32 ULID
    source: "ts-quickstart",
    content: "Lunaris bi-temporal memory — hello from TypeScript.",
    metadata: {},
    t_ref: null,
    bt: {
      valid: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
      sys:   [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
    },
  });
  console.log("ingested at LSN", lsn);

  const hits = await handle
    .recall()
    .top(5)
    .execute();

  for (const h of hits) console.log(h);
}

main();
```

Both examples target a running Moon backend at `moon://127.0.0.1:6380`.
Swap in `postgres://postgres:pass@localhost/lunaris` to run against
Postgres instead — Lunaris routes both URL schemes through the same
`StoragePort` trait.

## Three-surface pipeline toggles

`GraphPipeline` and `ConsolidatorPipeline` default to OFF (blueprint
§5.1 / §5.2). Flip them at any of three surfaces; resolution order is
**code > env > config** — code is always authoritative.

### Code surface

```python
# Python
handle.graph_pipeline.enable()
handle.consolidator_pipeline.disable()
```

```typescript
// TypeScript
handle.graphPipeline.enable();
handle.consolidatorPipeline.disable();
```

### Env surface

```bash
export LUNARIS_GRAPH_ENABLED=1
export LUNARIS_CONSOLIDATE_ENABLED=0
```

Read at `Lunaris::open` time via `initial_state_from_env()`.

### Config surface

```python
# Python — dict passed post-construction, walked by the `lunaris.open` wrapper.
handle = await lunaris.open(url, config={
    "graph_pipeline": {"enabled": True},
    "consolidator_pipeline": {"enabled": False},
})
```

```typescript
// TypeScript — opts object on the ergonomic open() wrapper.
const handle = await open(url, {
  graphPipeline: { enabled: true },
  consolidatorPipeline: { enabled: false },
});
```

## Backend URLs

| Scheme       | Backend          | Example                                         |
| ------------ | ---------------- | ----------------------------------------------- |
| `moon://`    | Moon (RediSearch)| `moon://127.0.0.1:6380`                         |
| `postgres://`| PostgreSQL       | `postgres://user:pass@localhost:5432/lunaris`   |

Both backends support the full Lunaris surface. Moon is faster on hot
recall paths; Postgres is the portability proof (zero ops overhead for
teams that already run Postgres). Within a single driver process,
`StoragePort::atomic_write` fans identical bytes to either backend, so
backend parity is structural by construction — see Plan 08-04's
`crates/lunaris-conformance/tests/run_bindings_backend_parity.rs` for
the per-driver parity gate.

## Troubleshooting

### "No matching distribution found for lunaris"

pip could not find a prebuilt wheel for your Python ABI / platform.
Options:

1. Check the target triple. `python -c "import sysconfig; print(sysconfig.get_platform())"` — this must match one of the 5 prebuilt targets above.
2. Source install: `pip install lunaris --no-binary lunaris` (requires Rust 1.94+).
3. File an issue — Lunaris tracks demoted targets in the release notes per ROADMAP risk register row 2.

### "undefined symbol: napi_get_value_*" at load time

Your Node runtime is older than NAPI 8 (Node 18 or lower). Upgrade to
Node 20 LTS or later. `abi_pin.spec.mts` catches this at test startup
with a readable reason.

### "embedding-gemma weights missing at /Users/…/Library/Caches/lunaris/…"

The default `Lunaris::open` path constructs a Candle `EmbeddingGemma`
embedder from `~/.cache/lunaris/models/embedding-gemma-300m/`. On a
fresh machine the cache is empty. Either:

1. Download the weights: `huggingface-cli download google/embeddinggemma-300m --local-dir ~/.cache/lunaris/models/embedding-gemma-300m/`
2. Use the Ollama embedder: the bundled Python + TS wheels are built with `default-features = false, features = ["ollama"]` precisely so `Lunaris::open` works without a weights cache; Ollama itself must be reachable at first embed call.

### Production package does not expose `conformance_fixture_episodes` / `scanKvPrefix`

Correct. Those helpers live behind the `bindings-it` Cargo feature and
are ONLY used by `crates/lunaris-conformance`'s per-driver backend-
parity tests. Production `pip install lunaris` / `npm install @pilotspace/lunaris`
wheels are built WITHOUT the feature so the surface stays minimal.

## Parity with the Rust crate

`cargo run -p lunaris-codegen -- --check` runs on every PR via the
`parity-check` CI job. Any drift between the annotated Rust surface
(`surface.toml`) and the committed Python (`generated_py.rs`) / TS
(`generated_ts.rs`) snapshots fails the PR. The `parity-check` gate
explicitly excludes handwritten `conformance.rs` files (Plan 08-02 /
08-03 / 08-04 carveout) so the feature-gated helpers can evolve
without triggering parity failures.

## Further reading

- Architecture: [`.planning/architect/blueprint.md`](../.planning/architect/blueprint.md)
- Requirements trace: [`.planning/REQUIREMENTS.md`](../.planning/REQUIREMENTS.md) — BIND-PY-01..05, BIND-TS-01..05, BIND-GEN-01..03
- Per-driver parity design: [`.planning/phases/08-sdk-bindings/08-04-PLAN.md`](../.planning/phases/08-sdk-bindings/08-04-PLAN.md)
- Rust user guide: [`guide.md`](./guide.md)
- Helios integration: [`helios-integration.md`](./helios-integration.md)
