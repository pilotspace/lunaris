# SPK-MOON-DEFAULT-MCP — Moon as the lunaris-mcp Default Storage

**Status:** SPIKE COMPLETE — one decision pending user confirm (see "The decision" below)
**Branch:** `feat/mcp-scratchpad-tools`
**Owns the design for:** milestone phase P-B — Moon as MCP default, auto-launched
**Date:** 2026-06-08

## Summary

Today `lunaris-mcp` defaults to a per-scope SQLite file
(`crates/lunaris-mcp/src/state.rs::resolve_storage_url` →
`sqlite:///<HOME>/.lunaris/<scope>.db`). P-B wants Moon to be the default —
auto-launched, data rooted at the working directory — with SQLite demoted to an
explicit `--storage sqlite://…` opt-out. Moon is what unlocks the graph, native
hybrid FT search, and the durability/recovery story; SQLite is the zero-deps
onboarding floor.

The interesting finding of this spike: **there are two ways to "auto-launch Moon
rooted at cwd," not one.** The prior session recorded the premise that "Moon is
network-only (no embedded/in-process mode)" and therefore P-B *must* spawn a
`moon` server subprocess. That premise is wrong — Moon ships an in-process
`run_embedded(config, cancel)` entry point. **The decision the user made
("auto-launch Moon, data dir = cwd") is unchanged and correct; what this spike
corrects is the recorded *rationale* — there is a second implementation, and it
has to be weighed, not assumed away.**

The headline result of weighing them: **in-process `run_embedded` still binds a
loopback TCP socket and the client still connects over the redis protocol** — so
in-process buys *single-binary distribution and nothing else* (no latency win, no
recall win). That single advantage attacks the subprocess path's single cost
(acquiring a `moon` binary), but that cost is already mostly solved by existing
machinery. Net recommendation: **supervised subprocess by default; keep
in-process `run_embedded` as a deferred-not-rejected optional build target.**

---

## The decision

> **When `lunaris-mcp` starts with no `--storage` override, how does it bring up
> the default Moon backend?**
>
> - **A (recommended): supervised subprocess.** lunaris-mcp spawns and supervises
>   a signed `moon` server process rooted at `./.lunaris-moon`, then connects over
>   loopback. Reuses the same binary users already get via brew / the release
>   tarballs. Process isolation; SQLite circuit-breaker fallback.
> - **B (deferred-not-rejected): in-process `run_embedded`.** lunaris-mcp links the
>   `moon` server crate (`--features runtime-tokio`), runs `run_embedded` on a
>   `tokio::spawn`, and connects over loopback. One binary, no subprocess — at the
>   cost of dragging the whole server crate into lunaris-mcp.
>
> Both satisfy "auto-launch Moon, data dir = cwd." The recommendation is A; this
> decision is surfaced to the user via AskUserQuestion when the spike lands.

Everything below is the evidence and reasoning behind that recommendation.

---

## Corrected premise (what changed since the milestone memory)

The milestone memory (`project_mcp_scratchpad_moon_milestone`) states under P-B:

> "Moon is NETWORK-ONLY (no embedded/in-process mode — `MoonClient::connect(...)`
> is the only entry; data dir is a Moon *server* property)."

That is factually wrong. `vendor/moon/src/server/embedded.rs` exposes:

```rust
#![cfg(feature = "runtime-tokio")]
pub async fn run_embedded(mut config: ServerConfig, cancel: CancellationToken)
    -> anyhow::Result<()>
```

It resolves the data dir, creates the persistence directory, spawns per-shard
threads + an AOF writer, and serves clients — **all inside the caller's process**.
So an in-process default IS possible. The *decision* (auto-launch, data dir =
cwd) stands; only the "must be a subprocess" justification was unsound.

---

## Verified facts (file:line)

### F1 — `run_embedded` exists, but is not "socketless"

`vendor/moon/src/server/embedded.rs`:

- Gated `#![cfg(feature = "runtime-tokio")]` (line 1) — only compiles under Moon's
  **tokio** runtime feature, not its default **monoio** runtime.
- **Still binds a loopback TCP listener** on `config.bind:config.port` and accepts
  `tokio::net::TcpStream` connections (≈ line 129). "In-process" means *no separate
  process/binary* — **not** zero-socket. The client connects over loopback exactly
  as it would to a subprocess.
- Console (`rust-embed` React assets), admin port, Prometheus, TLS, cluster, and
  SIGHUP are **feature-gated off / skipped** in this path (≈ lines 22–27). So the
  honest in-process bloat estimate is the **engine + storage + AOF + graph**, not
  the full distribution.

### F2 — Crux: both paths are loopback redis-protocol → no latency delta

Because F1's listener is loopback and the Lunaris client speaks redis over TCP in
both cases (`MoonClient` holds a `redis::aio::MultiplexedConnection`,
`vendor/moon/sdk/rust/src/client.rs:42`), **in-process and subprocess have the
same per-call cost.** The *only* thing in-process buys is single-binary
distribution. There is no recall/latency argument in either direction.

### F3 — lunaris-mcp does **not** depend on the moon server crate today

`crates/lunaris-mcp/Cargo.toml` depends on `lunaris`, `lunaris-core`,
`lunaris-retrieve`, `rmcp`, clap, tokio, serde, schemars, reqwest, indicatif.
Moon reaches lunaris-mcp **only transitively** through
`lunaris → lunaris-storage-moon → moondb (SDK)`. In-process `run_embedded` would
add a **heavy new direct dependency** on the entire `moon` *server* crate.

### F4 — SHA coupling: in-process *widens* an existing coupling

Per `reference_vendor_moon`, `lunaris` already path-depends on the vendored
`moondb` SDK, so `vendor/moon` SHA bumps already gate `lunaris-storage-moon`
compilation. In-process **extends** that coupling from the SDK to the full server
crate — strictly more surface that a SHA bump can break. (Widens, not introduces.)

### F5 — Signed, per-arch release binaries already exist

- `vendor/moon/install.sh` downloads from
  `https://github.com/pilotspace/moon/releases/download/${VERSION}/`.
- `vendor/moon/packaging/homebrew/moon.rb.tmpl` references per-arch tarballs
  `moon-v{{VERSION}}-{aarch64,x86_64}-{macos,linux-tokio}.tar.gz` + SHA256s.
- **The Linux release is the `-tokio` flavor.** So even the subprocess path runs
  *tokio-Moon* on Linux. The "monoio is faster" point is moot for both paths — and
  for a single-user stdio scratchpad MCP, per-call perf is a non-issue regardless.

### F6 — A subprocess launch prototype already exists in-repo

`scripts/bench-mcp-stdio.py::start_moon(binary, port, data_dir)` already spawns
Moon with the right args and a readiness gate:

```python
[binary, "--bind","127.0.0.1", "--port",str(port), "--admin-port","0",
 "--dir",str(data_dir), "--appendonly","no", "--shards","1"]
# then: tcp_ready("127.0.0.1", port, timeout_s=5.0) else terminate()/kill()
```

The Rust supervisor is a direct port of this shape (plus reuse-if-running and
Drop-driven shutdown). `scripts/test-recovery.py` (MOON_BIN) and
`scripts/setup-lunaris-agents.py --moon-bin` are additional references.

### F7 — Readiness contract: connect *is* the functional FT.CREATE probe

`MoonStorage::connect_with_dim` (`crates/lunaris-storage-moon/src/lib.rs:108`)
calls `ensure_indexes()`, which issues
`FT.CREATE lunaris_{scope}_{kind}_idx` for chunks/entities/facts/communities plus
`GRAPH.CREATE`. So the supervisor needs only a **two-tier** readiness check:

1. **TCP-accept** (port open) for the spawn/retry loop — cheap, matches the Python
   prototype's `tcp_ready`.
2. **`Lunaris::open(moon://…)`** then performs the real `FT.CREATE` handshake. A
   binary that is too old or built without the text-index feature fails *fast and
   loud* at open (the existing `reference_moon_local_run` "release build required"
   caveat). The supervisor surfaces that as an actionable "incompatible moon
   binary" error — no separate FT probe needed.

### F8 — Platform support: Moon is Linux + macOS only

Moon targets Linux (primary) and macOS; **no Windows**. Consequence:

- **Subprocess:** on Windows (or any host where no `moon` binary resolves), the
  supervisor falls back to the SQLite default — degraded but functional.
- **In-process:** lunaris-mcp built with the server crate **cannot be compiled for
  Windows at all** — it would split the build target matrix.

---

## Path A — supervised subprocess (recommended)

### Shape

`crates/lunaris-mcp/src/state.rs::resolve_storage_url` is the seam. When no
`--storage` override is present and the Moon-default is enabled, instead of
minting a `sqlite://` URL, the bootstrap asks a new `MoonSupervisor` for a live
`moon://127.0.0.1:<port>` URL.

New module (`crates/lunaris-mcp/src/moon_supervisor.rs`):

- **Binary resolution**, in order:
  1. `LUNARIS_MOON_BIN` env (operator override).
  2. Bundled binary next to the npx/uvx install (phase-26 distribution).
  3. Dev fallback `../moon/target/release/moon` (per `reference_moon_local_run`).
  4. **None found → circuit-break to SQLite** with a one-line warning (never hard-fail
     the MCP merely because Moon is unavailable).
- **Port allocation:** bind `:0` to grab a free loopback port, hand it to Moon.
- **Data dir:** `./.lunaris-moon` (cwd-rooted, the user's "data dir = cwd" intent).
- **Launch args:** from F6 (`--bind 127.0.0.1 --port <p> --admin-port 0 --dir
  ./.lunaris-moon --shards 1`; `--appendonly yes` for durability, unlike the bench
  harness which disables it).
- **Readiness:** F7's two-tier check — TCP-accept loop (timeout + bounded retries
  with backoff), then `Lunaris::open` as the functional FT.CREATE probe.
- **Reuse-if-running:** a port/pid lockfile under `./.lunaris-moon` lets a second
  MCP instance attach to the already-running Moon instead of double-spawning.
- **Graceful shutdown:** `Drop` / shutdown hook sends SIGTERM, waits with a
  timeout, then SIGKILL; cleans the lockfile. Orphan sweep on next start.

### Design-for-failure (CLAUDE.md: timeouts, retries, circuit breakers, rollback)

This is where the subprocess path is *strictly easier* than in-process:

- **Timeout:** bounded spawn-readiness deadline.
- **Retries:** N bounded spawn attempts with backoff.
- **Circuit breaker:** repeated spawn failure → fall back to the SQLite default.
  The MCP always comes up; it never bricks because Moon won't start.
- **Rollback:** the existing SQLite default *is* the rollback target — already
  tested, already the current behavior.
- **Isolation:** a Moon crash is a child-process exit the supervisor can detect and
  restart. (In-process, the equivalent is a panicked shard OS thread inside the MCP
  process — far harder to contain or recover.)

### The one real cost — binary acquisition — and why it's small

This is Path A's only genuine downside, and it is *already mostly solved*:

- **Signed releases exist** (F5).
- **Phase-26 (npx/uvx distribution) already has a download-tarball-with-sha256
  mechanism** (`postinstall.js` / the uvx path fetch `lunaris-mcp-{target}.tar.gz`
  from releases against a manifest). Extending it to *also* fetch the matching
  `moon-v{VER}-{arch}-{os}.tar.gz` is incremental, not novel.
- `LUNARIS_MOON_BIN` covers air-gapped/operator-managed installs.
- Dev fallback covers this repo's own workflow.

So the residual cost is "wire moon into the phase-26 downloader," not "invent a
distribution story."

---

## Path B — in-process `run_embedded` (deferred-not-rejected)

### Shape

lunaris-mcp takes a direct dependency on the `moon` server crate built
`--no-default-features --features runtime-tokio, text-index, …`, constructs a
`ServerConfig` (bind `127.0.0.1`, free port, dir `./.lunaris-moon`, 1 shard,
appendonly), `tokio::spawn`s `run_embedded(config, cancel_token)`, waits for the
loopback port, then `Lunaris::open(moon://127.0.0.1:<port>)`. Shutdown fires the
`CancellationToken`.

### Costs (the reason it is *not* the default)

- **Heavy new direct dep** on the full server crate (F3) — engine, AOF, graph,
  vector. (Console/admin/Prometheus are gated off per F1, so the estimate is honest,
  not worst-cased — but it is still the whole storage+engine.)
- **`runtime-tokio`, not monoio** — Moon's portability runtime, not its optimized
  default (moot for perf per F5, but it is the less-exercised build).
- **Widens vendor/moon SHA coupling** from the SDK to the full server crate (F4).
- **Windows build matrix split** (F8) — lunaris-mcp could no longer build for
  Windows with Moon compiled in.
- **In-process failure containment is harder** — a shard-thread panic now lives
  inside the MCP process; the clean kill+restart of Path A is unavailable.
- **Still binds a loopback socket** (F1/F2) — no latency or recall benefit to offset
  any of the above.

### The one real advantage

**Single binary.** No separate `moon` to ship or download — which directly attacks
Path A's one cost. That is the entire case for B, and it is real; it is just
outweighed once Path A's cost is shown to be mostly-solved (above).

---

## Deciding axis & recommendation

The fork reduces to a single axis:

> B's only advantage (single binary) ⟷ A's only cost (acquire a binary).

The chain that resolves it:

1. B's single-binary win attacks exactly A's binary-acquisition cost — that is the
   real trade, everything else is one-sided.
2. But A's cost is **already mostly solved**: signed releases (F5) + the phase-26
   downloader + `LUNARIS_MOON_BIN` + dev fallback. The win shrinks to "save one
   tarball fetch in the installer."
3. With that win small, the **one-sided** factors decide it, all favoring A:
   process isolation + clean kill/restart (CLAUDE.md design-for-failure), Windows
   portability (F8), keeping the full server crate *out* of lunaris-mcp (F3/F4),
   and SQLite as a tested circuit-breaker fallback.
4. Perf is **not** a factor either way (F2, F5).

**Recommendation:** default = **supervised subprocess (Path A)**. Record
in-process `run_embedded` as a **deferred-not-rejected** future *optional* build
target behind a `--features embedded-moon` flag — for environments that genuinely
want a single self-contained binary and accept the dep/runtime/Windows trade. This
mirrors how P-C (`SPK-CONSOLIDATE-MCP`) recorded a path forward without forcing a
premature implementation.

---

## Required before exposure (TDD-ready checklist)

Red/green discriminating tests (the production MCP bootstrap path must actually
exercise the supervisor — guard against "built ≠ wired", per
`feedback_built_not_wired`):

- [ ] **Moon-default round-trip:** MCP started with no `--storage` spawns Moon and a
      `scratchpad_write` → `scratchpad_read` round-trips through the auto-launched
      Moon (asserts the URL is `moon://…`, not `sqlite://…`).
- [ ] **Graceful fallback:** no resolvable binary → MCP still boots on SQLite with a
      warning (circuit breaker), **not** a crash.
- [ ] **Reuse-if-running:** a second MCP instance attaches to the running Moon
      (no double-spawn; lockfile honored).
- [ ] **Clean shutdown:** MCP exit / `Drop` terminates the child and leaves no
      orphaned process and no stale lockfile.
- [ ] **`LUNARIS_MOON_BIN` honored** over the dev/bundled fallback.
- [ ] **Opt-out intact:** `--storage sqlite://…` (and `memory://`) still bypass the
      supervisor entirely.
- [ ] **Incompatible-binary error** (F7): a non-text-index build surfaces an
      actionable error at `Lunaris::open`, not a silent zero-hit backend.

Implementation seam: `crates/lunaris-mcp/src/state.rs::resolve_storage_url`
(+ new `crates/lunaris-mcp/src/moon_supervisor.rs`). `bootstrap`'s existing
`probe_embedder_health` flow is unchanged; the supervisor slots in *before*
`Lunaris::open`.

## Open problems

- **Binary bundling for pip/npm** — extend the phase-26 (`postinstall.js` / uvx)
  download-tarball-with-sha256 mechanism to fetch the matching `moon` release. The
  *only* substantial follow-on, and it is incremental.
- **Default-on timing** — ship Moon-default behind a flag for one release (opt-in)
  before flipping it to the unconditional default, or flip immediately with the
  SQLite circuit-breaker as the safety net. (Recommend: flip immediately; the
  fallback makes it safe.)
- **Windows** — Moon-default degrades to SQLite there (F8); document it.

## Addendum (2026-06-08) — Path B selected; in-process feasibility VERIFIED

The user **chose Path B (in-process `run_embedded`)** over the subprocess
recommendation — accepting the dependency/runtime trade for single-binary
distribution. Before any code, the manifests were read to confirm Path B is
actually buildable. It is. Verified facts (these refine F1–F8):

- **Lib target + reachable entry.** `vendor/moon/Cargo.toml` has no `[lib]`
  override but `vendor/moon/src/lib.rs` exists with `pub mod server` (line 62),
  so the root `moon` crate produces a library that re-exposes
  `server::embedded::run_embedded` (gated `runtime-tokio`).
- **No allocator conflict.** The `#[global_allocator]` (mimalloc / jemalloc) is
  declared **only in `vendor/moon/src/main.rs`** (lines 24–30), not in any lib
  module. Linking the server *library* imposes no global allocator on
  lunaris-mcp — the one potential hard compile blocker is absent.
- **F3 CONFIRMED (not contradicted).** The workspace `moon` dependency is the
  *SDK*, not the server: root `Cargo.toml:161`
  `moon = { path = "vendor/moon/sdk/rust", version = "0.2.0", package = "moondb" }`
  (the lightweight `moondb` crate — deps: redis/tokio/thiserror/bytes/tracing).
  `vendor/moon` (the server) is **`exclude`d from the workspace**
  (`Cargo.toml:66–69`). So Path B is a genuine **new** heavy dependency, exactly
  as F3 stated. Every `use moon::` in the lunaris tree is the `moondb` alias.
- **Dependency shape for lunaris-mcp:** a path dep on the excluded crate, aliased
  to avoid colliding with the `moondb`→`moon` alias, e.g.
  `moon_server = { path = "../../vendor/moon", package = "moon",
  default-features = false, features = ["runtime-tokio", "graph", "text-index"] }`
  (`graph` for `GRAPH.CREATE`, `text-index` for the `FT.CREATE` Lunaris's
  `ensure_indexes` needs — F7).
- **Publishability is moot — but note the manifest correction.** lunaris-mcp's
  Cargo.toml carried an *explicit* `publish = true`, NOT the workspace default —
  however it is **not** in `scripts/topo_order.py`'s crates.io `PUBLISH` set (15
  library crates only) and ships solely as a prebuilt binary via the phase-26
  npx/uvx tarballs + `mcp-prebuild.yml` (npm `@pilotspace/lunaris-mcp` + PyPI
  wheel). The optional path-dep on the unpublished server crate would break a
  future `cargo publish`, so the P-B implementation (quick 260608-vuz) flipped it
  to `publish = false` — safe, because lunaris-mcp was never on crates.io. Nothing
  else depends on lunaris-mcp (leaf binary), so the dep is contained.
- **Real residual costs (land on CI, not users, since the binary is prebuilt):**
  `runtime-tokio` force-pulls `aws-lc-rs` (cmake/clang crypto build) even though
  `run_embedded` skips TLS; `mlua` (vendored Lua, needs a C compiler) is
  non-optional; `text-index`/`graph` add fst/stemmers/logos. Build time +
  C-toolchain grow materially. And F4's SHA coupling is now real for lunaris-mcp:
  a `vendor/moon` bump that breaks the *server* build breaks lunaris-mcp's build.
- **F8 CORRECTION (Windows).** `vendor/moon/Cargo.toml:91–93` documents
  *"Windows: Tokio is default (IOCP, work-stealing fallback)"* — Moon claims
  Windows support via the tokio runtime (the io-uring/nix/libc deps are
  unix/linux-`cfg`-gated). So Path B may actually **build on Windows** (unverified
  — no Windows release tarball ships today, F5 lists only macos/linux-tokio). The
  spike's "in-process splits the build matrix / Windows unsupported" argument
  against B is **weaker than stated**; this softens the anti-B case and is
  consistent with the user's choice.

**Verdict:** Path B is feasible with no hard blockers. Implementation seam is
still `resolve_storage_url`; the supervisor module becomes an in-process
`EmbeddedMoon` launcher (build `ServerConfig` → `tokio::spawn(run_embedded)` →
await loopback readiness → `Lunaris::open` = FT.CREATE probe → `CancellationToken`
on shutdown). The required-before-exposure TDD checklist above still applies
(swap "subprocess spawned" for "run_embedded task spawned"; "graceful fallback"
and "incompatible binary" become "feature-disabled fallback to SQLite").

## Related files

| File | Role |
|---|---|
| `crates/lunaris-mcp/src/state.rs` | `resolve_storage_url` — the seam to change |
| `crates/lunaris-mcp/src/main.rs` | `Cli { scope, storage }` — `--storage` opt-out already wired |
| `crates/lunaris-mcp/Cargo.toml` | deps today (no moon server crate — F3) |
| `vendor/moon/src/server/embedded.rs` | `run_embedded` — Path B entry (F1) |
| `vendor/moon/sdk/rust/src/client.rs:42` | `MoonClient` redis connection (F2) |
| `crates/lunaris-storage-moon/src/lib.rs:108` | `connect_with_dim` → `ensure_indexes`/FT.CREATE (F7) |
| `crates/lunaris/src/open.rs` | `moon://` URL dispatch |
| `scripts/bench-mcp-stdio.py::start_moon` | subprocess launch + readiness prototype (F6) |
| `vendor/moon/install.sh`, `packaging/homebrew/moon.rb.tmpl` | signed release tarballs (F5) |
| `.planning/phases/26-npx-uvx-distribution/` | phase-26 downloader to extend for the `moon` binary |
| `docs/spikes/SPK-CONSOLIDATE-MCP.md` | sibling spike (P-C); same deferred-but-scoped shape |
