# Running Lunaris against an external Moon — onboarding runbook

**Audience:** an operator who has never deployed Lunaris and needs a working
`lunaris-server` talking to a Moon instance.

**The supported deployment is an EXTERNAL Moon.** Lunaris does not embed one.
`lunaris-mcp` has an `embedded-moon` cargo feature, but it is a dev/test path
that is deliberately kept out of every default feature set and out of every
published binary (see `CLAUDE.md` → "embedded-moon — opt-in, never in
`default`"). Production means: you run a Moon, Lunaris dials it.

Everything below is grounded in the shipped source. Where a claim comes from a
file, the file and line are cited so you can re-derive it after a version bump.

Related runbooks:

* [`backup-restore.md`](backup-restore.md) — measured RPO/RTO, the cold-backup
  procedure, restore-to-a-new-host, and seven demonstrated failure modes. This
  document does **not** repeat it.
* [`observability.md`](observability.md) — what `/metrics` exposes, a
  Prometheus scrape config, and a starter alert set.

---

## 0. The four things that will bite you

Read these before anything else. Each is a hard failure, not a tuning nit.

| # | Rule | What happens if you get it wrong |
|---|------|----------------------------------|
| 1 | Moon must be **>= 0.8.5** | `Lunaris::open` fails at connect with an explicit "unsupported server version" error (§3) |
| 2 | Moon must run **`--shards 1`** | Every ingest fails: `TXN does not support cross-shard writes` (§5). The official Docker image defaults to `--shards 0` = auto-detect = sharded. |
| 3 | Moon must run **`--appendonly yes`** | No recovery authority. A restart loses everything the RDB path did not capture; `BGSAVE` produces no `dump.rdb` at all (§6). |
| 4 | The `moon://` URL carries **no credentials** and defaults to port **6380** | A `requirepass`-protected Moon is unreachable; an omitted port silently dials the wrong one (§4). |

---

## 1. Install Moon

Two supported shapes. Pick one.

### 1a. Binary — the official installer

```bash
# latest release
curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh | sh

# pinned (recommended for production)
curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh \
  | VERSION=v0.8.5 INSTALL_DIR=/usr/local/bin sh
```

The URL, the `VERSION` / `INSTALL_DIR` overrides, and the checksum-verified
download are all defined in `vendor/moon/install.sh:1-11` and
`vendor/moon/install.sh:114-132`. With no `VERSION` the script resolves the
latest tag through GitHub's `releases/latest` redirect
(`vendor/moon/install.sh:62-75`) — fine for a laptop, **pin it in production**.

Verify:

```bash
moon --version
```

### 1b. Container — the ghcr image

```bash
docker pull ghcr.io/pilotspace/moon:0.8.5
```

**The tag has no leading `v`.** Both publish workflows compute
`VERSION="${VER#v}"` and push `ghcr.io/${REPO}:${VERSION}`
(`vendor/moon/.github/workflows/release.yml:519,524`,
`vendor/moon/.github/workflows/docker-publish.yml:61,65`), so release tag
`v0.8.5` becomes image tag `0.8.5`. `:latest` moves only for stable releases —
a prerelease tag never repoints it.

Do **not** run the image with its baked-in command. See §5.

---

## 2. Start Moon correctly

### Binary / systemd

```bash
moon --bind 127.0.0.1 --port 6379 \
     --shards 1 \
     --appendonly yes --appendfsync everysec \
     --dir /var/lib/moon \
     --admin-port 9100
```

Flag provenance, all from `vendor/moon/src/config.rs`:

| Flag | Default | Line | Note |
|------|---------|------|------|
| `--shards` | `1` | `:213-223` | `0` means **auto-detect from CPU count**, i.e. sharded. Lunaris requires `1`. |
| `--appendonly` | `yes` | `:100-103` | The AOF is the recovery authority. |
| `--appendfsync` | `everysec` | `:137-138` | `always` for a ~0 RPO at a throughput cost. |
| `--dir` | auto-resolved | `:158-160` | Persistence files. Pass it explicitly. |
| `--protected-mode` | `yes` | `:250-252` | Rejects non-loopback connections when no password is set. Must be `no` for container networking — pair it with network isolation, since Lunaris cannot send a password (§4). |
| `--admin-port` | `0` = **disabled** | `:27-29` | Set it to enable the admin HTTP server: `/metrics`, `/healthz`, `/readyz` (`vendor/moon/src/admin/http_server.rs:138-148`). Required if you want to scrape Moon. |
| `--maxmemory` | unset | `:177-184` | Whole-instance cap. |
| `--profile standalone` | — | `:325-344` | Convenience preset that fills `--shards 1`, `--io-busy-poll-us 40`, `--io-driver epoll` for flags you left at their default. Safe on shared cores since 0.8.1. Optional. |

A packaged install ships `/etc/moon/moon.conf` +
`vendor/moon/packaging/moon.service` (`ExecStart=/usr/bin/moon
/etc/moon/moon.conf`). CLI flags override conf-file values
(`vendor/moon/packaging/moon.conf.example:15`).

### Container

Override `command`. The image's baked-in CMD is

```
moon --bind 0.0.0.0 --port 6379 --shards 0 --dir /data --protected-mode no
```

(`vendor/moon/Dockerfile:149`) — note `--shards 0`, and note that AOF is not in
the CMD at all (it relies on the `appendonly` default of `yes`,
`vendor/moon/src/config.rs:103`). Be explicit:

```bash
docker run -d --name moon \
  -v moon-data:/data \
  -p 127.0.0.1:6379:6379 -p 127.0.0.1:9100:9100 \
  ghcr.io/pilotspace/moon:0.8.5 \
  moon --bind 0.0.0.0 --port 6379 --shards 1 \
       --appendonly yes --appendfsync everysec \
       --dir /data --admin-port 9100 --protected-mode no
```

`/data` is the declared volume (`vendor/moon/Dockerfile:132`). Exposed ports
are 6379 (Redis protocol), 6443 (TLS), 9100 (admin)
(`vendor/moon/Dockerfile:136`).

---

## 3. Version: Moon >= 0.8.5 is enforced at connect

Lunaris issues **one** `INFO server` on every freshly-established Moon
connection and gates on the `moon_version` field
(`crates/lunaris-storage-moon/src/client.rs:413-468`). The floor is

```rust
pub const MIN_MOON_VERSION: MoonVersion = MoonVersion { major: 0, minor: 8, patch: 5 };
```

`crates/lunaris-storage-moon/src/version.rs:86`.

### What an operator sees on an old server

`Lunaris::open` returns `StorageError::Backend` and `lunaris-server` exits 1
with `Lunaris::open(moon://…) failed: …` (`main.rs:43-49`). The message is
verbatim (`client.rs:447-462`):

```
moon: unsupported server version — <host>:<port> reports moon_version 0.8.1,
but this Lunaris build requires >= 0.8.5. Older Moon builds are missing command
surface Lunaris depends on (FT.* vector/BM25 search, graph Cypher, TEMPORAL.*),
which would otherwise surface later as an opaque `ERR unknown command` in the
middle of a recall. To proceed: upgrade the server —
`curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh | VERSION=v0.8.5 sh`,
or run the ghcr.io/pilotspace/moon image at a tag >= v0.8.5 — then reconnect.
Confirm with `redis-cli -h <host> -p <port> INFO server | grep moon_version`.
```

### The fix

```bash
# 1. confirm what the server actually is
redis-cli -h 127.0.0.1 -p 6379 INFO server | grep moon_version

# 2. upgrade in place (binary install)
curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh \
  | VERSION=v0.8.5 INSTALL_DIR=/usr/local/bin sh
systemctl restart moon        # or: docker compose pull moon && docker compose up -d moon

# 3. re-check, then restart lunaris-server
redis-cli -h 127.0.0.1 -p 6379 INFO server | grep moon_version
```

**Upgrading the binary does not touch the data directory.** If you hit this
after a restore, fix the binary on the target host and leave the data alone
([`backup-restore.md` §6.5](backup-restore.md)).

### Handshake edge cases (deliberate, do not "fix")

Documented in `crates/lunaris-storage-moon/src/version.rs:59-75`:

| `INFO` reply | Outcome |
|---|---|
| `moon_version` < 0.8.5 | **hard error at connect** |
| `moon_version` >= 0.8.5 | `debug!`, connect proceeds |
| no / unparseable `moon_version` (e.g. a plain Redis) | `warn!` once per process, connect proceeds |
| server *rejected* `INFO` | `warn!` once, connect proceeds |
| server never *answered* `INFO` (timeout, dropped socket) | **hard error** — the connection itself is broken |

Pre-release suffixes are parsed off and discarded: `0.8.5-dev` compares equal
to `0.8.5` (`version.rs:96-99`), so running an off-tag build is fine.

The gate reads `moon_version`, never `redis_version` — Moon hard-codes the
latter to the compatibility literal `"7.4.0"`
(`vendor/moon/src/command/connection.rs:20`), which every plain Redis also
reports.

---

## 4. Point Lunaris at Moon

### URL form

```
moon://<host>:<port>[?ws=<workspace>][&quant=<tier>][&ef=<n>]
```

Parsed by `crates/lunaris-storage-moon/src/client.rs:293-324`.

**Two traps:**

1. **The default port is 6380, not 6379.** `DEFAULT_MOON_PORT` is `6380`
   (`client.rs:31`), while Moon's own default is `6379`
   (`vendor/moon/src/config.rs:24`). Omitting the port dials a port nothing is
   listening on. **Always write the port out.**
2. **Credentials in the URL are silently ignored.** The client rebuilds the
   dial string as `format!("redis://{host}:{port}")` (`client.rs:341`) — any
   `user:pass@` you put in the `moon://` URL is dropped on the floor. A Moon
   started with `--requirepass` is therefore **not reachable** from Lunaris
   today. Isolate Moon at the network layer instead: bind to loopback or a
   private network, never publish 6379 to the internet.

### Configuration surface

Every `lunaris-server` flag has a `LUNARIS_*` twin
(`crates/lunaris-server/src/config.rs`). CLI flags win over env vars.

| Env var | Flag | Default | Source |
|---|---|---|---|
| `LUNARIS_STORAGE` | `--storage` | *(required)* | `config.rs:79` |
| `LUNARIS_BIND` | `--bind` | `0.0.0.0:8080` | `config.rs:76` |
| `LUNARIS_TOKENS_FILE` | `--tokens-file` | *(required)* | `config.rs:82` |
| `LUNARIS_RATE_PER_SECOND` | `--rate-per-second` | `60` | `config.rs:85` |
| `LUNARIS_RATE_BURST` | `--rate-burst` | `120` | `config.rs:88` |
| `LUNARIS_CORS_ORIGINS` | `--cors-origins` | `*` | `config.rs:91` |
| `LUNARIS_SHUTDOWN_GRACE_SECS` | `--shutdown-grace-secs` | `30` | `config.rs:94` |
| `LUNARIS_HTTP_TIMEOUT_SECS` | `--http-timeout-secs` | `30` (`0` = off) | `config.rs:105` |
| `LUNARIS_HTTP_CONCURRENCY` | `--http-concurrency` | `256` (`0` = off) | `config.rs:111` |
| `LUNARIS_MOON_OP_TIMEOUT` | — | `10` (seconds, per Moon command) | `client.rs:809` |
| `LUNARIS_EMBEDDER_GGUF` | — | `~/.lunaris/models/…Q4_K_M.gguf` | `crates/lunaris/src/handle.rs:1902` |
| `LUNARIS_RERANKER_GGUF` | — | `~/.lunaris/models/…Q5_K_M.gguf` | `crates/lunaris/src/handle.rs:1905` |

The tokens file is a JSON map (`middleware/auth.rs:4-6`):

```json
{ "<bearer-token>": { "tenant": "<scope-id>", "scopes": ["ingest", "recall", "forget"] } }
```

`ingest`, `recall`, `forget` are the only three scope strings any route
requires (`lib.rs:152-250`; every read route — `/v1/snapshot/{lsn}`,
`/v1/episode/{id}`, `/v1/scopes`, `/v1/browse/{kind}`, `/v1/detail/{kind}/{id}`,
`/v1/graph` — is gated on `recall`). The `tenant` value is the **only** source
of truth for the storage partition; wire-side `scope` fields are ignored.

### Start the server

```bash
lunaris-server \
  --storage moon://127.0.0.1:6379 \
  --tokens-file /etc/lunaris/tokens.json \
  --bind 0.0.0.0:8080
```

Confirm:

```bash
curl -fsS localhost:8080/healthz   # {"ok":true,"version":"…"}   liveness
curl -fsS localhost:8080/readyz    # {"ready":true,"checks":{…}}  readiness
```

`/healthz` is a bare storage PING; `/readyz` additionally runs a **write
canary** (`KvPut` + `KvDelete` on `lunaris:__health__:canary`, 2 s budget) and
an embedder check (`crates/lunaris-server/src/readiness.rs`). Use `/healthz`
for a Kubernetes `livenessProbe` and `/readyz` for the `readinessProbe` —
restarting the process does not un-wedge a downstream store
(`routes/readyz.rs:9-14`).

---

## 5. Single shard is a correctness requirement, not a preference

> A sharded Moon is **not a Lunaris backend**. Not "less tested" — not usable.

Lunaris commits each episode as **one** cross-key Moon `TXN` (the INGEST-04
single-`atomic_write` invariant). Moon rejects a TXN that spans shards, so on a
`--shards > 1` instance the very first ingest fails:

```
storage: backend: moon: redis error: ResponseError:
  TXN does not support cross-shard writes -- use hash tags {tag} to co-locate keys
```

This is measured, not theoretical — the backup/restore drill run at
`--shards 4` never reaches the backup step. Full write-up:
[`backup-restore.md` §6.6](backup-restore.md) (and §7, "Single shard — and that
is the only shape Lunaris supports").

**The trap:** Moon's *binary* default is `--shards 1`
(`vendor/moon/src/config.rs:223`), but the *container image* CMD passes
`--shards 0` = auto-detect from CPU count (`vendor/moon/Dockerfile:147-149`).
An operator who runs the image as-shipped on a 4-core host gets a 4-shard Moon
and a Lunaris that cannot write a single episode.

Check a running instance:

```bash
redis-cli -h 127.0.0.1 -p 6379 INFO | grep -i shard
```

Changing shard count on an existing data directory is a Moon-side migration
(`moon --migrate-aof-from … --migrate-aof-shards N`,
`vendor/moon/src/config.rs:820-840`) — not something to do casually under a
live Lunaris.

---

## 6. Persistence — what the backup runbook assumes

The [backup/restore runbook](backup-restore.md) is written against a Moon
running:

```
--shards 1 --appendonly yes --appendfsync <everysec|always>
```

Three facts that decide whether your backups are real:

1. **The AOF is the recovery authority.** A Lunaris backup is the *whole*
   Moon data directory. Copying a subset is data loss
   ([`backup-restore.md` §1, §6.2](backup-restore.md)).
2. **`BGSAVE` is the wrong verb.** On a Moon running `--appendonly yes`,
   `BGSAVE` produced **no `dump.rdb` at all** (measured;
   [`backup-restore.md` §6.2](backup-restore.md)), and even when a `dump.rdb`
   exists it does not participate in AOF replay
   ([`docs/durability.md:142`](../durability.md)). The verb that anchors the
   AOF chain is **`BGREWRITEAOF`**, which folds the incremental log into
   `appendonlydir/moon.aof.<N>.base.rdb` and collapses your RTO.
3. **The restore host must run the same durability flags** as the source
   ([`backup-restore.md` §3 step 4](backup-restore.md)) **and** a Moon binary
   `>= MIN_MOON_VERSION`, or Lunaris refuses at connect (§3 above, and
   [`backup-restore.md` §6.5](backup-restore.md)).

Do not re-derive the procedure here — run it from
[`backup-restore.md` §2](backup-restore.md), and validate it with

```bash
scripts/backup-restore-drill.sh --docs 200
```

---

## 7. Model weights (embedder + reranker)

`lunaris-server` built with the `llamacpp` feature loads two GGUFs at runtime:

* embedder `granite-embedding-311m-multilingual-r2` Q4_K_M (768-d)
* reranker `bge-reranker-v2-m3` Q5_K_M (lazy, on first recall)

resolved from `LUNARIS_EMBEDDER_GGUF` / `LUNARIS_RERANKER_GGUF`, else
`~/.lunaris/models/` (`crates/lunaris/src/handle.rs:1876-1905`).

**A missing GGUF is not a startup failure.** `open()` logs a `WARN` banner and
falls back to a zero-vector `NoopEmbedder` — the server comes up healthy and
every vector recall returns **zero rows** (`docs/guide.md:104`). This is the
single most common "Lunaris returns nothing" report. Prove you did not ship it:

```bash
ls -lh "${LUNARIS_EMBEDDER_GGUF:-$HOME/.lunaris/models}"/*.gguf
# and after startup, grep the log for the fallback banner:
journalctl -u lunaris-server | grep -i noopembedder
```

Stage the artifacts (SHA-256s are printed by the helper):

```bash
cargo run -p lunaris-bench --bin stage-models -- --help
```

There is a second, quieter way to ship a `NoopEmbedder`: build the server
**without** the feature. See §8.

---

## 8. Building `lunaris-server`

```bash
git submodule update --init vendor/moon        # mandatory — see below
cargo build --release -p lunaris-server --features lunaris/llamacpp
```

Two non-obvious requirements:

* **The `vendor/moon` submodule must be checked out.** The Moon SDK is a
  *path* dependency (`Cargo.toml:158`,
  `moon = { path = "vendor/moon/sdk/rust", version = "0.2.1", package = "moondb" }`).
  Inside the workspace the path always wins over the crates.io release, so a
  missing submodule fails the build with
  `failed to read vendor/moon/sdk/rust/Cargo.toml`.
* **`--features lunaris/llamacpp` is required for a production build.** The
  workspace pins `lunaris = { …, default-features = false }`
  (`Cargo.toml:101`) and `lunaris-server` takes it with `workspace = true`
  (`crates/lunaris-server/Cargo.toml:19`), so a bare
  `cargo build -p lunaris-server` yields a **Tier-0** binary with
  `lunaris-llamacpp` absent from the graph entirely — `NoopEmbedder`, zero-row
  vector recall. Verified with:

  ```bash
  cargo tree -p lunaris-server --features lunaris/llamacpp -i lunaris-llamacpp
  # lunaris-llamacpp -> lunaris-memory -> lunaris-server
  cargo tree -p lunaris-server -i lunaris-llamacpp
  # error: package ID specification `lunaris-llamacpp` did not match any packages
  ```

  The feature compiles llama.cpp, so the build host needs **cmake**, a **C++
  toolchain**, and **libclang** (bindgen, via `llama-cpp-sys-2`) — the same
  three the release workflows install
  (`.github/workflows/python-prebuild.yml:126-138`).

GPU offload is a *build-time* feature (`metal` / `cuda` / `vulkan`,
`crates/lunaris/Cargo.toml`), not a runtime switch.

---

## 9. Docker Compose — the whole thing in one file

[`deploy/docker-compose.yml`](../../deploy/docker-compose.yml) brings up a
correctly-flagged Moon (pinned tag, `--shards 1`, AOF on, named volume) plus a
`lunaris-server` built from
[`deploy/Dockerfile`](../../deploy/Dockerfile), with `/readyz` as the
lunaris-server healthcheck.

```bash
cd deploy
cp .env.example .env                    # pin MOON_VERSION, set host paths
cp tokens.example.json tokens.json      # replace the placeholder tokens
mkdir -p models                         # or set LUNARIS_MODELS_DIR in .env
docker compose up -d --build

curl -fsS localhost:8080/readyz
docker compose logs -f lunaris-server
```

Notes on the compose file, so you can adapt it rather than copy it blindly:

* **No `lunaris-server` image is published anywhere.** The crate is
  `publish = false` (`crates/lunaris-server/Cargo.toml:8`) and no workflow
  builds a container for it, so the service uses a `build:` stanza with
  `context: ..` (the build needs the workspace *and* `vendor/moon`).
* **Moon's compose healthcheck is `moon --check-config`**, inherited from the
  image (`vendor/moon/Dockerfile:141-142`) because the distroless runtime has
  no shell and no HTTP client. It validates configuration in a separate
  process — it does **not** prove the listener is up or that writes are
  accepted. The probe that catches a write-stall wedge is Lunaris' own
  `/readyz` canary. If you want a real Moon probe, curl its admin port
  (`/healthz` on `--admin-port`, `vendor/moon/src/admin/http_server.rs:138`)
  from a sidecar or from your monitoring system.
* **Moon's ports are not published to the host** by default — the protocol is
  unauthenticated (§4). Uncomment the `ports:` block only on a trusted host.
* **`stop_grace_period` must exceed `LUNARIS_SHUTDOWN_GRACE_SECS`**, or Docker
  SIGKILLs the process before its bounded drain finishes
  (`crates/lunaris-server/src/shutdown.rs::serve_with_deadline`).
* **`~` is not expanded** in compose volume paths. `LUNARIS_MODELS_DIR` must be
  absolute or relative to `deploy/`.
* **Compose does not pull the image at `config` time.** The compose file was
  validated with `docker compose config`; the ghcr image itself has not been
  pulled or run from this repo.

---

## 10. Quick triage

| Symptom | Likely cause | Where |
|---|---|---|
| `unsupported server version — … requires >= 0.8.5` | old Moon binary | §3 |
| `TXN does not support cross-shard writes` on first ingest | `--shards > 1` (often the image default `--shards 0`) | §5 |
| Connection refused / `moon connect timed out` | wrong port — `moon://host` with no port dials **6380** | §4 |
| Auth errors from Moon, or nothing works with `--requirepass` set | Lunaris' `moon://` URL carries no credentials | §4 |
| `/healthz` 200 but `/readyz` 503 with `canary: timeout` | Moon accepts connections but stalls writes (the wedge signature) | §4, [observability.md](observability.md) |
| Recall returns zero rows, no errors | missing GGUF, **or** a build without `lunaris/llamacpp` | §7, §8 |
| Restart lost data | `--appendonly no`, or a backup taken with `BGSAVE` | §6, [backup-restore.md](backup-restore.md) |
| `failed to read vendor/moon/sdk/rust/Cargo.toml` at build | submodule not checked out | §8 |
