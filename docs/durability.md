# Durability & Crash Recovery

Status: alpha. Complements `docs/guide.md`. Baseline claims validated against
live Moon on 2026-04-23 — rerun `scripts/test-recovery.py` to re-verify. §2.1,
§2.4, §2.5 updated 2026-07-10 for the Moon v0.5.1+ substrate bump
(moon-v051-perf-exploit, `vendor/moon` @ `c9508066`); §2.7 added 2026-07-15
for the Moon v0.7.1 bump (moon-v070-bump, `vendor/moon` @ `4161cdc`); §2.8
added 2026-07-16 for the Moon v0.8.0 bump (moon-v080-bump, `vendor/moon` @
`e41aa671` = 0.8.0 + PR #351). The SDK (`moondb 0.2.1`) is API-identical
across all three bumps, so every change below is server-side behavior, not a
Lunaris wire-format change.

Lunaris is stateless: every byte of durable state lives in the backend (Moon or Postgres). Recovery is therefore a backend concern. This guide documents the Moon-backed path, the recovery procedure, the two live-measurement gotchas you need to know, and how to test recovery yourself.

---

## 1. What survives a crash

| entity | persisted in | survives Moon crash? | survives Lunaris client crash? |
|---|---|---|---|
| Episode body (KV) | Moon HSET at `lunaris:episode:<ulid>` | yes — via AOF + RDB | n/a (client-side is stateless) |
| Chunk text (KV) | Moon HSET at `lunaris:chunk:<ulid>` | yes | n/a |
| Vector (768-d) | Moon `chunks:<hex>` HSET + FT HNSW | yes | n/a |
| BM25 index | Moon FT inverted index | yes (rebuilt from HSETs on replay) | n/a |
| Graph (entities, facts) | Moon `GRAPH.*` storage | yes | n/a |
| Pipeline queue (consolidate, verify) | Moon Streams | yes | n/a |
| In-flight ingest that wasn't `await`-ed | — | no (never committed) | no (never committed) |

Every user-visible write goes through `atomic_write` (see `crates/lunaris-storage-moon/src/atomic.rs`). The ingest umbrella splits a single `Episode` into one batch of `WriteOp` values (KV puts + vector upserts + optional graph writes) and ships them inside one atomic envelope. Moon commits the envelope as a unit — partial envelopes never appear in the AOF.

---

## 2. Moon persistence model (for operators)

Moon combines **AOF (append-only file)** with **base RDB snapshots**:

1. Every write is appended to `<dir>/appendonlydir/moon.aof.<N>.incr.aof`. With `--appendfsync always` the append is `fsync`'d on every command — no data loss on `kill -9` after a successful `await ingest()`.
2. `BGREWRITEAOF` (or the auto-save rules in `--save "<seconds> <changes>"`) rotates the AOF: it writes a **base RDB** snapshot at `<dir>/appendonlydir/moon.aof.<N+1>.base.rdb`, then starts a new empty incremental at `moon.aof.<N+1>.incr.aof`.
3. On restart, Moon loads the newest `base.rdb` into memory, then replays the matching `incr.aof` on top. The FT indices are rebuilt from the replayed HSETs automatically.

### 2.1 Launching Moon for durability

```bash
../moon/target/release/moon \
  --bind 127.0.0.1 --port 6380 \
  --dir /var/lib/moon \                    # where AOF + RDB live
  --shards 1 \
  --appendonly yes \                       # enable AOF
  --appendfsync always \                   # fsync every write (default: everysec)
  --save "3600 1 300 100"                  # RDB auto-rewrite rules
```

The `--save` rules follow the Redis convention: `seconds changes` pairs — here, rewrite if either 1 change happened in 3600 s OR 100 changes in 300 s. Pair them with an explicit `BGREWRITEAOF` before planned shutdowns if you want a clean snapshot.

**Everything above is still accurate on Moon v0.5.1+.** `--wal-kv-log` (§2.1)
changes what feeds the *WAL* (a separate, per-shard checkpoint/replication
log — see §2.4), not the AOF/RDB rotation Lunaris' `BGREWRITEAOF` guidance
depends on. Do not confuse the WAL with the AOF: AOF is still the crash-
recovery authority whenever `--appendonly yes` is set, and `BGREWRITEAOF`
still produces the `appendonlydir/moon.aof.<N>.base.rdb` anchor snapshot
the base-RDB trap below is about.

### 2.1 AOF+WAL KV double-write eliminated (`--wal-kv-log`, v0.5.1)

Before v0.5.1, every KV write with `--appendonly yes` was logged to **both**
the per-shard AOF and the per-shard WAL — measured 2.7× file-byte / 4.1×
device write amplification at `--shards 4`, even though startup recovery
always wipes WAL-replayed state and replays the AOF (the WAL copy was pure
disk wear on the write path Lunaris' `atomic_write` hot-loops on).

`--wal-kv-log auto|on|off` (default **`auto`**) controls this:

- **`auto`** (default, what Lunaris ships): KV records are skipped from the
  WAL while the AOF is the recovery authority (`--appendonly yes`) and no
  CDC subscriber is attached. Logging re-engages dynamically the moment a
  CDC subscriber attaches, so point-in-time/CDC consumers still see every
  write — there is no window where they'd miss records.
- **`on`**: pre-0.5.1 behavior — always log KV records to the WAL. Set this
  if you run `--appendonly no` (see below) and still want a WAL-based
  point-in-time recovery / full CDC history, since `auto` would otherwise
  have no durability log to fall back to.
- **`off`**: never log KV records to the WAL (FPI/checkpoint/feature records
  are unaffected either way). With `--appendonly no` **and** `--wal-kv-log
  off`, there is NO KV durability log at all — every write is memory-only.

Lunaris' embedded-Moon launcher (`crates/lunaris-mcp/src/embedded_moon.rs`)
does not set `--wal-kv-log` at all, so it inherits the `auto` default —
verified by
`embedded_moon::tests::server_config_new_v051_flags_have_sane_defaults`.
Never force it to `on` for a Lunaris deployment unless you have a CDC
consumer or a specific PITR requirement — `auto` gives the same crash-
recovery guarantee at a fraction of the write amplification.

### 2.2 The base-RDB trap

**Without an existing base RDB, AOF-only state is unreplayable.** If you start Moon fresh, ingest data, and `kill -9` before the first auto-save / `BGREWRITEAOF` has run, the restart fails with:

```
Error: multi-part AOF replay failed
  AOF base RDB missing at moon.aof.1.base.rdb but incr moon.aof.1.incr.aof
  is 821715 bytes; refusing to replay incr against empty state
```

This is a deliberate Moon invariant — the AOF chain needs an anchor snapshot, it won't silently replay against an empty state.

**Safe patterns:**

1. Let the auto-save rules run naturally — `--save "60 1"` forces a base RDB within 60 s of any write, small enough for dev boxes.
2. Run `BGREWRITEAOF` explicitly after large bulk ingests; poll for the new `moon.aof.<N>.base.rdb` to appear on disk before you consider the data "durable".
3. Before a planned restart, run `BGREWRITEAOF` + wait for the file.

```bash
# Force a base RDB and wait for it before trusting durability
redis-cli -p 6380 BGREWRITEAOF
while ! ls /var/lib/moon/appendonlydir/*.base.rdb >/dev/null 2>&1; do
  sleep 0.2
done
```

> **Do not use `BGSAVE` for this.** `BGSAVE` writes `<dir>/dump.rdb`, which is a separate artefact that does NOT participate in AOF replay. Only `BGREWRITEAOF` produces the `appendonlydir/moon.aof.<N>.base.rdb` that Moon's recovery chain needs.

### 2.2b Post-replay ranking permutation (measurement gotcha, not a durability issue)

After a restart the FT/HNSW index is rebuilt from replayed HSETs, whose
insertion order can differ from the original — near-tie hits may come back
in a different ORDER even though the recalled SET is byte-identical
(observed on v0.7.1, 2026-07-15; the recovery harness asserts set identity
and prints an `order-drift` note when this happens). Downstream rerankers
absorb this; don't write tests that assert exact post-restart hit order.

### 2.3 FT index lag (measurement gotcha, not a durability issue)

`await kb.ingest(...)` returns after the atomic envelope is ACKed. But Moon's FT index `num_docs` can lag the underlying HSETs by a few hundred milliseconds while the index materialises. If you snapshot `FT.INFO chunks` immediately after the last ingest and then crash, post-recovery `num_docs` can appear higher than the pre-crash snapshot — because the index caught up during the interim.

This is not data loss. The underlying HSETs are already fsync'd. The workaround in tests is to `sleep` 1–2 s before taking the "pre-crash" snapshot. In production you don't need to do anything — the data is durable either way.

### 2.4 AOF backpressure is now fail-loud, not silent (v0.5.1)

Before v0.5.1, a full AOF writer channel (sustained write pressure, slow
disk) logged a `warn!` and **dropped the record while the client still
received `+OK`** — a client-acked write that Lunaris believed was committed
could silently vanish on the next AOF replay (AOF/memory divergence).

v0.5.1 durable handler paths now await the enqueue under
`--aof-fsync-timeout-ms` and surface the failure as an error frame instead:
the synchronous SPSC-drain and inline-SET paths apply one bounded blocking
send (a single 5 ms budget shared across a whole pipeline/MULTI batch), and
on loss, the response is replaced with **`MOONERR AOF backpressure`** instead
of an ack. Every drop is `error!`-logged and counted in the
**`aof_backpressure_dropped`** counter exposed via `INFO persistence`.

**Operational impact for Lunaris:** `atomic_write` propagates this as a
`StorageError::Backend` from the Moon backend — the ingest pipeline's
existing `?`-propagation (no silent partial commit) already surfaces it
correctly; no Lunaris-side code change was required. Operators SHOULD poll
`aof_backpressure_dropped` alongside `chunks_num_docs` in health checks —
a nonzero and climbing counter means the disk can't keep up with the write
rate, not that data has been silently lost (the new behavior guarantees the
opposite: a client only sees `+OK` when the AOF write has actually been
durably enqueued).

### 2.5 Vector-index restart durability — no full re-index (v0.5.1 / B1-B3)

Before this work, every Moon restart discarded all in-memory vector index
state and paid a **full re-index** (TQ/SQ8 encode + HNSW build) of every
matching hash key by re-scanning the entire keyspace — for a large `chunks`
index this dominated cold-start time.

Vector indexes now **persist their segments across restarts**. Each index
gets an `idx-<hex(name)>/` directory holding:

- an atomically-written `manifest.json` (collection_id, segment ids,
  id-allocator floors),
- a checksummed `keymap-<epoch>.bin` (key_hash → global_id + vector
  checksum + original key), covering every indexed key (mutable + immutable)
  at the moment of the snapshot,
- the immutable HNSW segments themselves (staged write:
  `staging-<id>` → fsync → atomic rename, written in the background after
  each compact/GraphUnion merge install).

On restart, Moon **loads this state instead of discarding it**: segments are
read back and reattached (pinning the index's `collection_id` to the
persisted value so the HNSW QJL rotation seed matches), and the keyspace
rescan becomes a **dedup rescan**: a key whose vector bytes checksum-match
the last snapshot is rebuilt as metadata-only (no HNSW/TQ/SQ8 re-encode);
only genuinely changed or unknown keys are fully re-indexed; keys removed
from the keyspace since the snapshot are tombstoned.

**Crash-safety contract:** any corrupt/missing artifact (segment, checksum
mismatch, unreadable header) degrades to a rescan-rebuild of exactly the
affected keys — never to wrong search results. A manifest may *understate*
what's durable (costing an extra rescan) but never *overstates* it. Startup
also sweeps orphaned segment/staging/keymap files and stale `idx-*`
directories left by an interrupted drop.

**Known gap:** for multi-vector-field indexes, only the *default* vector
field's segments/checksums are persisted — additional named vector fields
always re-encode on restart (documented, conservative — Lunaris only uses
the default field per index today, so this doesn't affect the
`chunks`/`entities`/`facts`/`communities` indexes).

**Operational impact for Lunaris:** embedded-Moon (`embedded_moon.rs`) and
any standalone Moon deployment now have materially faster restart-to-ready
time proportional to *changed* keys since the last snapshot, not total
keyspace size — relevant for the MCP server's in-process launch path, which
previously paid the full re-index cost on every process restart.

### 2.6 RSS memory watchdog (`--mem-full-pct`, Wave 3)

The memory analogue of the disk-free guard (§2 launch example uses
`--disk-free-min-pct`, MA12): Moon now pauses writes once process RSS
crosses `--mem-full-pct` percent of the detected system/cgroup memory limit
(default **95**), and resumes only once RSS drops to `mem_full_pct - 5`
(hysteresis, prevents flapping). Unlike `--maxmemory` (which can be an
unconfigured 0), this fires on *actual* RSS vs the detected limit — a
meaningful backstop for the disk-starved/RAM-constrained hosts this project
already runs live-Moon tests on. Read-only commands are never blocked; like
the diskfull guard, DEL/UNLINK/EXPIRE/FLUSHALL are write-flagged and blocked
too (no allowlist). Set to `0` to disable. Embedded-Moon does not override
this flag, so it inherits the 95% default — verified by
`embedded_moon::tests::server_config_new_v051_flags_have_sane_defaults`.

### 2.7 WAL v3 — atomic durable writes, FTS term-dict durability, upgrade safety (v0.7.0/v0.7.1)

The Moon v0.7.1 bump (`vendor/moon` @ `4161cdc`, 2026-07-15) hardens the
per-shard WAL to **WAL v3** and closes the upgrade-replay hole that mattered
most to Lunaris:

- **Atomic durable writes.** A WAL v3 record either commits durably as a
  whole or is discarded on replay — the same all-or-nothing property
  Lunaris's single-`atomic_write` ingest envelope (INGEST-04) already
  depends on at the AOF layer now also holds at the WAL layer.
- **FTS term-dictionary durability.** The FT term dictionary is persisted
  with the WAL instead of being rebuilt best-effort, removing a
  keyword-recall regression window after hard crashes.
- **#69 upgrade safety (`segment_plane_scan`).** v0.6.0 wrote MQ and
  temporal plane records inside a nested-Command framing; v0.7's plane scan
  initially skipped them, which would have silently dropped **MQ backlog +
  PEL state and temporal history** on a v0.6→v0.7 restart. The v0.7.1
  binary unwraps the nested framing — and Lunaris pins this with a
  dedicated harness leg:

  ```bash
  # old binary writes KV+graph+MQ+temporal probes, v0.7.1 replays the dir
  python scripts/test-recovery.py --upgrade-replay \
    --old-bin ~/.lunaris/bin/moon \
    --new-bin vendor/moon/target/release/moon
  ```

- **v0.7.1 patch notes.** (a) SQ8/TQ code-size mis-dispatch fix (#73): on
  SQ8-quantized indexes (Lunaris's opt-in `LUNARIS_MOON_QUANT=sq8` config)
  the memory-accounting hot path logged an error per call and pegged a CPU
  core; v0.7.1 computes the true SQ8 layout and latches the error log.
  (b) Deterministic replica TTL (#71): relative expiries are rewritten to
  absolute deadlines before entering the durable log / replication stream,
  and replicas no longer run their own expiry sweeps — an AOF replay after
  restart now reproduces the master's expiry *instant* instead of
  restarting the countdown. Both matter for the roadmapped replica
  read-split (Tier 3).

The kill-9 recovery harness (`scripts/test-recovery.py`) also gained
raw-command **MQ + temporal plane probes** in TEST 1 — previously only KV,
FT indices, and recall identity were asserted across a crash; now an
un-ACKed MQ backlog (with a consumed head) and a bi-temporal `v`+`bt` hash
must replay byte-identically too.

### 2.8 One Storage Kernel GA — kill-9-lossless on every plane (v0.8.0)

The Moon v0.8.0 bump (`vendor/moon` @ `e41aa671` = tag 0.8.0 + PR #351,
2026-07-16) graduates the WAL v3 work to **One Storage Kernel GA**: every
plane (KV, vector, graph, MQ, temporal) is now covered by upstream's own
scheduled **crash-matrix CI** (#352), complementing the Lunaris harness.
What the bump carries for Lunaris:

- **GraphUnion merge backoff (#353).** Rejected auto-merges (recall below
  `MERGE_RECALL_TOLERANCE`) now back off exponentially instead of retrying
  the same segment pairs forever. This cures the abort-merge CPU livelock
  observed live on this host 2026-07-14→16 (continuous
  `merge recall < tolerance` warns, and — combined with the MA1 global
  segment-count stall guard — a post-restart total write refusal). With
  #353 the `--max-unflushed-immutable-segments 4096` operator override
  should become unnecessary; verify the backlog drains before retiring it.
- **DashTable recovery-panic fix (PR #351 — why the pin is a main SHA,
  not the tag).** Stock v0.8.0 still carries the
  `double NeedsSplit after split_segment` unreachable: loading a large
  shard checkpoint (`shard-0.rrdshard`) with hash-skewed keys panicked
  deterministically and crash-looped the daemon 119× on this host. The
  pinned commit adds a split-retry loop (parity with `insert`); the
  regression test + the quarantined production checkpoint both prove it.
- **Disk-offload hardening (#349/#350).** `used_memory` is truthful under
  disk-offload (RSS→logical ledger) and spill segments batch (~129× fewer
  heap files) — both directly relevant on disk-starved hosts like this one.

Upgrade validation for this bump: the `--upgrade-replay` leg runs
v0.7.1-stock as the writer and the v0.8 binary as the replayer (all five
planes), plus a poisoned-checkpoint boot gate (`dashtable_regression`)
against the quarantined 2026-07-16 state.

---

## 3. Recovery procedure

### 3.1 Moon process crashed (kill-9, OOM, power loss)

```bash
# 1. Confirm Moon is really dead on the port
lsof -nP -iTCP:6380 -sTCP:LISTEN   # expect nothing

# 2. Inspect the AOF state
ls /var/lib/moon/appendonlydir/
# Healthy: one or more moon.aof.<N>.base.rdb files + matching .incr.aof
# Dangerous: only .incr.aof files, no .base.rdb → restart will refuse

# 3. Restart with the same --dir
../moon/target/release/moon --bind 127.0.0.1 --port 6380 \
  --dir /var/lib/moon --shards 1 --appendonly yes --appendfsync always \
  --save "60 1" &

# 4. Wait for ready
while ! redis-cli -p 6380 PING | grep -q PONG; do sleep 0.1; done

# 5. From the Lunaris side, just reconnect
python -c "import asyncio, lunaris; asyncio.run(lunaris.open('moon://127.0.0.1:6380'))"
```

Expected replay cost: ~20 ms per 1 MB of AOF on darwin-arm64. A 50-doc / ~820 KB AOF replays in 0.90 s end-to-end (measured pre-v0.5.1, when every restart paid a full FT re-index). On v0.5.1+ the FT vector portion of that number is now a dedup rescan (§2.5) rather than a full TQ/SQ8 encode + HNSW rebuild — restart-to-ready time should be equal or faster for an unchanged keyspace; re-run `scripts/test-recovery.py` for a current number before citing it as a live SLA.

### 3.2 Lunaris client process crashed

Nothing to recover on the client side — the process was stateless. Any ingest that hadn't `await`-ed its result was never committed. Just re-run the script.

### 3.3 Neither side's AOF has a base RDB

If `ls /var/lib/moon/appendonlydir/` shows only `.incr.aof` files and Moon refuses to start, you have three options:

1. **Recoverable via synthetic replay.** Write a tiny script that: starts a fresh Moon on a scratch dir, re-plays the `.incr.aof` entries via a custom RESP client, then runs `BGREWRITEAOF`. (Requires reading `.incr.aof` format — see `moon/src/persistence/aof/`.)
2. **Treat as data loss.** Wipe the corrupt data dir, start fresh, re-ingest from source.
3. **File a bug on Moon.** Ideally Moon would offer a `--allow-unrooted-aof-replay` flag for the base-less case.

Preventing this is easier than recovering: always run `BGREWRITEAOF` after your first batch of writes when seeding a new deploy.

---

## 4. Testing recovery

A ready-to-run harness ships at [`scripts/test-recovery.py`](../scripts/test-recovery.py). It covers three failure modes:

| test | what it verifies |
|---|---|
| `test_moon_kill` | SIGKILL Moon, restart, recover. Asserts dbsize, FT index list, `chunks_num_docs`, and top-10 text identity for 5 probe queries all match pre-crash. |
| `test_lunaris_kill` | SIGKILL a child python ingest at the halfway mark. Asserts Moon state is self-consistent (no torn writes), FT.SEARCH still runs. |
| `test_write_after_restart` | After `test_moon_kill`, writes a fresh doc with a unique anchor phrase and verifies it roundtrips through search. |

Run it:

```bash
cd crates/lunaris-py
LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
  uv run --with datasets --with python-ulid --with redis \
    python -u ../../scripts/test-recovery.py --docs 50
```

Reference numbers from the 2026-04-23 run (50 docs, darwin-arm64, release Moon):

- Moon restart + AOF replay: 0.90 s wall
- Probe-identity check: 5/5 top-10 lists byte-identical
- Post-restart write roundtrip: PASS

Evidence log: [`milestones/v0.1.1-bench/recovery-test.log`](../milestones/v0.1.1-bench/recovery-test.log).

---

## 5. Known limitations

- **Single-shard only in this tested config.** Multi-shard Moon recovery semantics (cross-shard coordinator, manifest-level replay) aren't exercised by the harness yet.
- **No Postgres recovery harness.** Postgres handles its own durability (WAL + fsync); a symmetric test is BENCH-04 on the roadmap.
- **AOF grows unboundedly without auto-rewrite.** Use `--save` rules or schedule `BGREWRITEAOF` — otherwise replay time grows linearly with write volume.
- **Pipeline workers replay idempotently** (`consolidator`, `verifier`) — they read from Moon Streams on start-up and resume from the last committed offset. No action needed on the Lunaris side.
- **`scripts/test-recovery.py` predates the v0.5.1+ substrate bump** (§2.1, §2.4, §2.5). It still exercises the right failure modes (kill-9, restart, probe-identity) but its reference numbers (§4) were captured before the vector-index dedup rescan and AOF-backpressure fail-loud changes — re-run it and update §4's numbers rather than trusting the 2026-04-23 figures for anything restart-time or backpressure related.
- **Multi-vector-field indexes re-encode fully on restart** (§2.5) — only the default vector field's segments/checksums persist. Not a Lunaris-visible gap today (one vector field per index), but relevant if a future recipe adds a second named vector field to an existing index.

---

## 6. Related references

- `.planning/architect/blueprint.md` §5.4 — durability contract
- `crates/lunaris-storage-moon/src/atomic.rs` — the `WriteOp` envelope shape
- `crates/lunaris-mcp/src/embedded_moon.rs` — in-process Moon launch config; `parse_from([])` clap-default derivation (databases/shards/wal-kv-log/mem-full-pct/io-busy-poll-us/disk-offload all inherit upstream defaults)
- `vendor/moon/src/persistence/` — AOF + RDB implementation
- `vendor/moon/CHANGELOG.md` — `[0.5.1]` (AOF+WAL double-write elimination, AOF backpressure fail-loud) and `[Unreleased]` (vector-index restart durability B1-B3, RSS watchdog) sections are the source for §2.1/§2.4/§2.5/§2.6
- `docs/guide.md` §ingest + §recall — end-user API the recovery guarantees apply to

---

If something here doesn't match the source, the source wins. File an issue or update this doc.
