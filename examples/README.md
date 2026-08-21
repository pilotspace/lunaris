# Lunaris examples

Every project here is built or typechecked on every push by
[`.github/workflows/examples.yml`](../.github/workflows/examples.yml). None of
them was, before 2026-08-21 — which is why the three quickstarts spent a whole
minor version telling readers to migrate a relational schema that had been
deleted in 0.7.0, using an image that no longer existed.

## Before you start: you need a Moon, and there is no download yet

**0.7.0 is Moon-only.** Lunaris refuses any Moon below `0.8.5` at connect, and
**v0.8.5 was published with zero release assets** — every platform tarball
404s, and an anonymous `docker pull ghcr.io/pilotspace/moon:0.8.5` gets a
`401`. The v0.8.4 binaries that do exist are rejected by the handshake.

Two paths work today:

- **`lunaris try`** — no server, no config, no account. Runs ingest, indexing
  and recall inside one process against an in-process Moon. See
  [`docs/quickstart-try.md`](../docs/quickstart-try.md). (It has to be built
  from source too — `cargo build --release -p lunaris-cli --features embedded-moon`
  — because there is no prebuilt `lunaris` binary yet either.)
- **Build Moon from `vendor/moon`** — the recipe is in
  [`quickstart-rs/README.md`](quickstart-rs/README.md), "Path B". This is the
  only Moon 0.8.5 in existence right now.

Re-cutting the v0.8.5 assets is ship-plan task W0.1. Until it lands, no example
here can be run from published artifacts alone, and none of them pretends
otherwise.

## The examples

| Example | What it shows | Needs |
|---|---|---|
| [`quickstart-rs/`](quickstart-rs/) | **canonical.** open → ingest → scoped `recall()`, plus the typed DSL form | a single-shard Moon; a C++ toolchain (the default build embeds llama.cpp) |
| [`quickstart-py/`](quickstart-py/) | the same flow via `pip install lunaris` | a single-shard Moon |
| [`quickstart-ts/`](quickstart-ts/) | the same flow via `npm i @pilotspace/lunaris` | a single-shard Moon; Node 20+ |
| [`multi-agent-rs/`](multi-agent-rs/) | hard scope isolation between two agents, multiple sessions inside one agent (via `source`), resume across a process boundary | a single-shard Moon; **no** toolchain or model — it uses `StubEmbedder` |
| [`langgraph-lunaris/`](langgraph-lunaris/) | Lunaris as a LangGraph `BaseStore` | `lunaris-server` + a Moon behind it |
| [`crewai-lunaris/`](crewai-lunaris/) | Lunaris as CrewAI RAG storage | `lunaris-server` + a Moon behind it |
| [`letta-lunaris/`](letta-lunaris/) | Lunaris as a Letta archival connector shim | `lunaris-server` + a Moon behind it |

The Rust quickstart is canonical; the Python and TypeScript variants mirror its
shape so the API translation is obvious.

`multi-agent-rs` is the cheapest one to run: `default-features = false` means
no cmake, no C++, and no 253 MB GGUF — just a Moon.

## Known gap: the DSL is Rust-only

`quickstart-rs` shows two recall forms — the `scoped.recall(query)` one-liner
and the typed DSL tree
(`scoped.dsl().with_root(Vector::new("chunks", 30).top(5))`). **Only the
one-liner has a working SDK equivalent.**

`ScopedLunaris.dsl()` exists in both bindings and is a dead end in both: it
returns the codegen-frozen native `RetrievalBuilder`, whose combinators raise
`NotImplementedError` / throw, and which exposes no `execute()` at all. In
TypeScript it is worse than useless — it *typechecks*, because `lunaris.d.ts`
shadows `RetrievalBuilder` with the ergonomic class that does have `.query()`
and `.execute()`, so the compiler will not stop you writing code that fails at
runtime.

The builder that works, `handle.recall().query(q).top(n).execute()`, has no
scope parameter, so it reads a different partition than a scoped ingest wrote.
Net: **there is no scope-bound DSL path in the Python or TypeScript binding
today.** Both SDK quickstarts therefore stop at `scoped.recall(text)` and say
so in their READMEs.

## What CI checks, and what it cannot

[`examples.yml`](../.github/workflows/examples.yml) runs four gates. There is no
`|| true` anywhere in it.

| Gate | Covers | Catches |
|---|---|---|
| `cargo check --locked` | `quickstart-rs`, `multi-agent-rs` | compile breakage, API drift, **and** dependency drift — each example is its own workspace with its own lockfile |
| `tsc --noEmit` | `quickstart-ts` | type-level API drift against this commit's `crates/lunaris-ts/lunaris.d.ts` |
| `mypy` + `import` | `quickstart-py`, and the three framework examples | wrong method names, wrong kwargs, wrong arity, missing symbols |
| [`ci/no-dead-backends.sh`](ci/no-dead-backends.sh) | every file under `examples/` | live instructions naming a backend deleted in 0.7.0 |

That last one is a grep, and it exists because **no compiler and no type
checker can read a URL string or a shell block in a README.** The v0.6-era
examples type-checked perfectly while every one of their run instructions
pointed at a backend removed in 0.7.0. A gate that only compiles code would
have stayed green through the entire defect.

The gates still cannot tell you an example *works end to end* — that needs a
live Moon, which needs W0.1. Until then, "compiles and typechecks against the
real surface" is the honest ceiling, and it is what these jobs assert.
