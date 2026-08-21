# Lunaris Quickstart — TypeScript

Mirrors the [Rust](../quickstart-rs/README.md) and [Python](../quickstart-py/README.md)
quickstarts against the same single-shard Moon: `open` → `scoped.ingest` →
`scoped.recall`.

**0.7.0 is Moon-only.** The relational backends were deleted; if you are coming
from 0.6.x see [`docs/migration/0.6-to-0.7.md`](../../docs/migration/0.6-to-0.7.md).

## You need a Moon binary, and there isn't a download yet

Lunaris 0.7.0 refuses any Moon below `0.8.5`, and **v0.8.5 was published with
zero release assets** — the tarballs 404 and the ghcr package answers `401` to
an anonymous pull. Read
[the Rust quickstart's section on this](../quickstart-rs/README.md#read-this-before-you-start-you-need-a-moon-binary-and-there-isnt-a-download-yet)
first; it has the two paths that do work.

## Prerequisites

- Node 20+
- a single-shard Moon on `127.0.0.1:6380`
- the granite-r2 Q4_K_M GGUF staged at `~/.lunaris/models/` (the prebuilt
  `.node` binding embeds llama.cpp; override with `LUNARIS_EMBEDDER_GGUF`)

## Three steps

```bash
cd examples/quickstart-ts

npm install

export LUNARIS_STORE_URL="moon://127.0.0.1:6380"
npm start                    # = npx tsx quickstart.mts
```

There is no migration step and no role bootstrap — Moon needs neither.

Expected output:

```
quickstart: opening lunaris handle at moon://127.0.0.1:6380
quickstart: ingested episode at lsn=1746000000000:1 under scope `quickstart`
quickstart: recalled 1 hit(s) for "hello"
quickstart:   top hit score=0.83 text="# Hello from Lunaris ..."
```

## Typecheck it without a server

```bash
npm install
npm run typecheck
```

`tsconfig.json` maps `@pilotspace/lunaris` to `../../crates/lunaris-ts/lunaris.d.ts`,
so this compiles the example against **this commit's** declarations rather than
whatever npm last published. That is the gate CI runs; see
`.github/workflows/examples.yml`. Delete the `paths` block to typecheck against
an installed release instead.

## Local-dev variant (building the binding from this repo)

```bash
cd ../../crates/lunaris-ts
npm run build                # napi build → .node prebuild
cd ../../examples/quickstart-ts
npm install ../../crates/lunaris-ts
npx tsx quickstart.mts
```

## The surface this uses

| | |
|---|---|
| `Scope.new("quickstart")` | validating newtype, `^[A-Za-z0-9_\-.]{1,128}$`; throws on a bad string |
| `handle.scoped(scope)` | scope-bound view; every operation carries the partition key |
| `new EpisodeBuilder(source, content)` | scope-less payload builder — `ingest` stamps the scope |
| `await scoped.ingest(builder)` | returns the string-formatted `Lsn` |
| `await scoped.recall(text)` | returns hit objects (`text`, `score`, `source`, `id`, …), filtered to this scope |

Two rough edges you will meet immediately, both visible in `quickstart.mts`:

- `LunarisHandle.scoped()` is declared `(scope: unknown) => unknown` in the
  hand-written `lunaris.d.ts`, so the result needs a cast to `ScopedLunaris`.
- `ScopedLunaris.recall()` is declared `Promise<any>` by the generated binding,
  so narrow it at the call site rather than letting `any` spread.

## Where the DSL stops

The Rust quickstart also shows a typed DSL form
(`scoped.dsl().with_root(Vector::new("chunks", 30).top(5))`). **That has no
working TypeScript equivalent**, and this one is a trap worth spelling out:

- `scoped.dsl()` returns the codegen-frozen native `RetrievalBuilder`
  (`crates/lunaris-ts/src/generated.rs`). Its combinators throw, and it has no
  `execute()`.
- It nonetheless **typechecks**, because `lunaris.d.ts` re-declares
  `RetrievalBuilder` with the ergonomic class that does have `.query()` /
  `.execute()`. The compiler will happily accept
  `scoped.dsl().query(q).execute()`; it fails at runtime.
- The builder that works is `handle.recall().query(q).top(5).execute()` — but
  it has no scope parameter, so it reads a different partition than this
  script wrote.

Use `scoped.recall(text)` for scoped retrieval, and read
[`examples/quickstart-rs/`](../quickstart-rs/) for the full DSL walkthrough.

## Troubleshooting

- `Cannot find module '@pilotspace/lunaris'` → `npm install`, or
  `npm install ../../crates/lunaris-ts` for local dev.
- `error: set LUNARIS_STORE_URL=...` → the script refuses to guess a store URL.
  Guessing would let it write demo episodes into whatever Moon owns that port.
- An `unsupported Moon version` handshake rejection → your Moon predates 0.8.5.
- Recall returns nothing / a `WARN` about the embedder → the granite-r2 Q4_K_M
  GGUF isn't staged. Download it to `~/.lunaris/models/` (SHA-256s:
  `cargo run -p lunaris-bench --bin stage-models -- --help`) or pass
  `EmbedderConfig.llamacpp({ ggufPath })` to `open`.
- `TXN does not support cross-shard writes` → Moon is sharded. It must run with
  `--shards 1`.
