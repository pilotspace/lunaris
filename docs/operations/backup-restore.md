# Backup & Restore-to-a-New-Host — operator runbook

Status: **validated**. Every number and every warning below was produced by
[`scripts/backup-restore-drill.sh`](../../scripts/backup-restore-drill.sh) on
2026-08-15 against a release Moon **v0.8.5** built from the `vendor/moon` pin
(`ab4c393e`), driven by the real Lunaris ingest and recall paths. Re-run the
drill after any Moon bump and update the tables here — do not carry these
figures forward on trust.

Companion documents:

* [`docs/durability.md`](../durability.md) — what survives a *crash* on the
  same host (AOF/RDB model, `scripts/test-recovery.py`). **This** document
  covers moving the data somewhere else.
* [`docs/deployment-tiers.md`](../deployment-tiers.md) — where Moon runs.

---

## 1. What a Lunaris backup actually is

Lunaris is stateless. Every durable byte lives in the storage backend, so
"backing up Lunaris" means **backing up the Moon data directory** (`--dir`).
Nothing on the Lunaris side needs to be captured: no local caches, no index
sidecars, no client state.

A Moon data directory contains, at minimum:

```
/var/lib/moon/
├── appendonlydir/
│   ├── moon.aof.<N>.base.rdb     # anchor snapshot for the AOF chain
│   ├── moon.aof.<N>.incr.aof     # appended writes since that anchor
│   └── moon.aof.manifest         # which base + incr pair is current
├── shard-0/                      # per-shard checkpoints, vector index segments
├── shard_0/
├── replication.state             # master_replid — the instance's identity
└── moon.lock
```

**Back up the whole directory.** Copying a subset — most tellingly, copying an
RDB snapshot alone — is total data loss (§6.2).

---

## 2. The supported procedure — cold (quiesced) backup

This is the only procedure this project endorses. It requires a write pause.

```bash
MOON_PORT=6381
MOON_DIR=/var/lib/moon
BACKUP=/backups/moon-$(date -u +%Y%m%dT%H%M%SZ)

# 1. Stop sending traffic to Lunaris (drain the server / scale to zero).

# 2. Anchor the AOF chain and WAIT for the rewrite to finish.
#    Poll both signals — `aof_rewrite_in_progress` reads 0 in the gap before
#    the child is forked, and a .base.rdb appears on disk before it is whole.
redis-cli -p $MOON_PORT BGREWRITEAOF
while :; do
  inprog=$(redis-cli -p $MOON_PORT INFO persistence | tr -d '\r' \
           | sed -n 's/^aof_rewrite_in_progress://p')
  base=$(ls -l "$MOON_DIR"/appendonlydir/*.base.rdb 2>/dev/null | awk '{s+=$5} END{print s+0}')
  [ "${inprog:-0}" = "0" ] && [ "${base:-0}" -gt 64 ] && break
  sleep 0.2
done

# 3. Shut Moon down cleanly and confirm the port is released.
redis-cli -p $MOON_PORT SHUTDOWN || true
while lsof -nP -iTCP:$MOON_PORT -sTCP:LISTEN >/dev/null 2>&1; do sleep 0.2; done

# 4. Copy the whole data directory.
cp -R "$MOON_DIR" "$BACKUP"

# 5. Checksum it so a silent transfer corruption is caught at restore time.
( cd "$BACKUP" && find . -type f -exec shasum -a 256 {} + | sort -k2 ) > "$BACKUP.sha256"

# 6. Bring Moon back up on the source host and resume traffic.
```

Step 2 is not about correctness on Moon ≥ 0.8.5 (see §6.4) — it is about
**RTO**: it collapses the incremental AOF into a compact base snapshot so the
restored instance replays a snapshot instead of a command log.

### 2.1 Backup verification (do this, not `PING`)

A backup that has never been restored is a hypothesis. Restore it somewhere
disposable and check **content**, not liveness:

```bash
scripts/backup-restore-drill.sh --docs 200
```

---

## 3. Restore onto a NEW host

```bash
NEW_DIR=/var/lib/moon
BACKUP=/backups/moon-20260815T041500Z

# 1. Verify the transfer.
( cd "$BACKUP" && find . -type f -exec shasum -a 256 {} + | sort -k2 ) \
  | diff - "$BACKUP.sha256" || { echo "backup is corrupt"; exit 1; }

# 2. Place the directory. NEVER restore on top of a directory a Moon is using.
cp -R "$BACKUP" "$NEW_DIR"

# 3. If the SOURCE instance is still alive anywhere (clone, not failover),
#    drop the replication identity so the two do not share a master_replid.
rm -f "$NEW_DIR/replication.state"          # see §6.3

# 4. Start Moon with the SAME durability flags as the source.
moon --bind 127.0.0.1 --port 6381 --dir "$NEW_DIR" \
     --shards 1 --appendonly yes --appendfsync always --save "3600 1 300 100"

# 5. Wait for ready.
while ! redis-cli -p 6381 PING | grep -q PONG; do sleep 0.1; done

# 6. Verify CONTENT, not liveness (see §3.1).
```

The restored Moon must be **≥ the version Lunaris requires**
(`MIN_MOON_VERSION`, `crates/lunaris-storage-moon/src/version.rs` — 0.8.5
today). Restoring a modern data directory under an older binary makes Lunaris
fail the version handshake at connect; the data is fine, the client refuses.

### 3.1 Content verification after restore

Liveness proves nothing. Compare a count *and* a sample of content against
what the source held:

```bash
# key count
redis-cli -p 6381 DBSIZE

# per-scope document count in the chunk index
redis-cli -p 6381 FT.INFO lunaris_<scope>_chunks_idx | grep -A1 '^num_docs$'

# real recall through Lunaris (the drill's workload driver does exactly this)
cargo run -p lunaris-memory --no-default-features --release \
  --example backup_restore_workload -- \
  verify --url moon://127.0.0.1:6381 --scope <scope> --docs <n> --out after.json
```

---

## 4. Measured RPO

RPO = how much acked data a restore can lose.

| procedure | RPO | measured |
|---|---|---|
| **Cold backup (§2)** — quiesce → BGREWRITEAOF → clean SHUTDOWN → copy | **0** for every write acked before the shutdown | 200-doc and 1000-doc corpora restored with `RPO_docs_lost = 0`, DBSIZE identical, and every chunk text recalled verbatim |
| Same, but the copy predates the BGREWRITEAOF | 0 on Moon ≥ 0.8.5 | the un-anchored copy restored intact, at the same RTO (§6.4) — the historical base-RDB trap does not fire on this version |
| **Hot `cp -R` under write load** (NOT supported) | everything acked from the instant `cp` starts reading, unbounded upward | **392 of 400** acked documents lost in run A, **1 996 of 2 000** in run B — and the restored instance reported no error at all |
| **RDB-only backup** (`BGSAVE` + copy `dump.rdb`) | **100%** | 0 documents recovered, DBSIZE 0, no error |

Between backups the RPO is simply the backup interval. Moon's
`--appendfsync always` guarantees zero loss on a **crash of the source host**
(see `docs/durability.md`); it does nothing for a lost host, which is what a
backup is for.

---

## 5. Measured RTO

RTO here = **restore-copy → Moon ready → first correct Lunaris recall**, i.e.
everything after you have the backup bytes on the target host. It excludes
network transfer of the backup, which dominates in reality and which you should
measure on your own link.

Two full drill runs, darwin-arm64, local APFS, Moon v0.8.5, `--shards 1`,
`--appendfsync always`:

| | run A | run B |
|---|---|---|
| documents ingested | 200 | 1 000 |
| Moon keys (`DBSIZE`) | 1 201 | 6 001 |
| backup size on disk | 3.89 MB | 19.25 MB |
| restore: copy backup → target path | 0.029 s | 0.076 s |
| restore: process start → `PONG` | 0.197 s | 0.327 s |
| restore: `PONG` → first correct recall (`settle`) | 0.003 s | 0.013 s |
| **RTO total (copy → verified recall)** | **0.351 s** | **0.804 s** |
| documents lost | 0 | 0 |

Supporting numbers from the same runs:

| | run A | run B |
|---|---|---|
| `BGREWRITEAOF` wall | 0.222 s | 0.182 s |
| clean `SHUTDOWN` wall | 0.245 s | 0.341 s |
| `incr.aof` before rewrite | 1.86 MB | 9.32 MB |
| `base.rdb` after rewrite | 1.88 MB | 9.42 MB |
| `incr.aof` after rewrite | 0 B | 0 B |

RTO on this substrate is dominated by neither AOF replay nor index rebuild — it
is essentially `cp` time plus process start. Moon ≥ 0.5.1 persists vector index
segments (`docs/durability.md` §2.5), so a restore does **not** pay a full HNSW
rebuild; the first correct recall lands within milliseconds of the port opening.

Scale by **bytes**, not documents: at 5× the documents the RTO rose 2.3×, roughly
tracking the 5× size growth against a fixed ~0.15 s process-start floor.

> **Measured, and counter to expectation: `BGREWRITEAOF` bought no RTO here.**
> Restoring the *un-anchored* copy (same data, no rewrite) came up in 0.193 s /
> 0.289 s versus 0.197 s / 0.327 s for the anchored one — replaying a 9.3 MB
> command log costs the same as loading a 9.4 MB base snapshot at this scale,
> and on a write-once corpus the rewrite compacts nothing. Keep the step
> anyway: on a long-lived, overwrite-heavy deployment the `incr.aof` grows
> without bound between rewrites and replay time grows with it. Do not cite it
> as an RTO control at small scale.

---

## 6. Failure modes (each one demonstrated, not assumed)

### 6.1 Hot `cp -R` of a live data directory — silent partial backup

The drill's leg 2 ingests an anchored baseline, starts a writer, and runs
`cp -R` while writes are in flight. Result, both runs:

* the restored instance **starts cleanly** — no error, no warning, `PING` is
  `PONG`, `FT._LIST` is complete;
* the anchored baseline corpus is fully intact, so a spot check on old data
  passes;
* **everything acked after the copy began is gone** — 392 of 400 documents in
  run A, 1 996 of 2 000 in run B — and nothing announces it. Note how little
  the copy's own duration (0.06–0.09 s) has to do with the size of the loss:
  the cut is at the instant `cp` reaches the growing `incr.aof`, and every
  write after that point is simply absent.

This is the worst failure shape there is: a backup that looks healthy and is
not. `cp -R` reads `appendonlydir/`, `shard-0/` and `replication.state` at
different instants with no cross-file consistency guarantee. **Do not take hot
copies with `cp`/`rsync`.** If you cannot take downtime, use an *atomic*
filesystem snapshot (LVM, ZFS, EBS snapshot) and copy from the snapshot.

### 6.2 RDB-only backup — total silent loss

`BGSAVE` on a Moon running with `--appendonly yes` produced **no `dump.rdb` at
all** (`bgsave_produced_dump_rdb=0`). An operator following Redis muscle memory
therefore backs up nothing, and the restore comes up as a perfectly healthy,
perfectly empty instance: `DBSIZE 0`, 0 documents recalled, no error. The
AOF-chain files under `appendonlydir/` are the recovery authority; the standalone
`dump.rdb` artefact does not participate in AOF replay.

### 6.3 `replication.state` carries the source's identity into the restore

Moon persists `master_replid` in `<dir>/replication.state`, so a verbatim
restore comes up claiming to be the *same* master as the source — the drill
pins this (`replication identity carried into a verbatim restore`). For a
**failover** (source is gone) that is what you want. For a **clone** (source
still running), two live instances sharing a replid is a replication hazard.

Removing `replication.state` before first start yields a fresh id and costs no
data — both asserted by the drill.

### 6.4 The "base-RDB trap" is stale for Moon ≥ 0.8.5

`docs/durability.md` §2.2 warns that a data directory with no `*.base.rdb`
anchor is unreplayable. **That no longer reproduces on v0.8.5**: Moon writes a
10-byte empty-state `moon.aof.1.base.rdb` at first boot, so even an
un-anchored copy restores intact (drill: "un-anchored copy ALSO restores
intact"). The drill re-checks this every run and will say so if a future Moon
brings the trap back.

`BGREWRITEAOF` therefore stays in the runbook for a different reason — RTO. It
folds the incremental log into the base snapshot; at 200 documents that moved
1.86 MB of `incr.aof` into a 1.88 MB `base.rdb` with `incr` back to 0 bytes.

### 6.5 Version-handshake refusal after a restore

Lunaris hard-fails at connect when `moon_version < MIN_MOON_VERSION`
(0.8.5 today). Symptom on a fresh host: Moon is up, `redis-cli PING` works, and
every Lunaris client errors at open. Fix the *binary* on the target host; do not
touch the data.

### 6.6 A sharded Moon is not a Lunaris backend at all

Since 0.7.0 you will not get this far: **Lunaris refuses to connect to a
multi-shard Moon**, with an error naming `--shards 1`
(`crates/lunaris-storage-moon/src/shards.rs`). Before that guard existed, running
the drill at `--shards 4` never reached the backup step — the very first ingest
failed:

```
storage: backend: moon: redis error: ResponseError:
  TXN does not support cross-shard writes -- use hash tags {tag} to co-locate keys
```

Lunaris commits each episode as **one** cross-key Moon TXN (the INGEST-04
single-`atomic_write` invariant), and Moon rejects a TXN that spans shards.

**Do not read that error's advice as a fix.** Hash tags do *not* make sharded
ingest work. [RFC 0008](../rfcs/0008-sharded-moon-ingest.md) §2.3 measured it:
the `TXN.*` guard is *"every key must land on **the connection's own shard**"*,
not *"the keys must agree with each other"* — and Moon assigns connections to
shards round-robin with no client control and no way to query the count. A
fully hash-tagged keyspace still failed on 11 of 16 scopes on one connection,
and would fail on a *different* 11 on the next.

And even a fixed write side would not give a usable backend, because the read
side is broken independently (RFC 0008 §1.3): `FT.NAVIGATE` — the graph leg of
recall — answers from the connection's own shard and never scatter-gathers, so
a Navigate for a scope living elsewhere returns **empty, with no error**. A
sharded deployment would write correctly and silently lose graph recall, which
is worse than today's loud failure.

So `--shards > 1` is not a "backup is unvalidated there" gap — it is a backend
Lunaris will not open. Run Moon with `--shards 1`. The drill detects the
mismatch and dies with the explanation rather than a confusing cascade.

### 6.7 Restoring onto a directory in use

`moon.lock` exists but the drill does not rely on it as a guard. Always restore
into a path no Moon process is holding, and start the new instance yourself.

---

## 7. Known limits of this validation

* **Single shard — and that is the only shape Lunaris supports.** The drill
  runs `--shards 1` because Lunaris refuses to connect to anything else (§6.6,
  [RFC 0008](../rfcs/0008-sharded-moon-ingest.md)). Multi-shard backup/restore
  is untested and moot until Moon scatter-gathers `FT.NAVIGATE`.
* **Single host, local filesystem.** "New host" is modelled as a fresh data
  directory, a fresh port, and a new server process. Cross-machine transfer,
  its checksums, and its wall time are the operator's to measure.
* **No point-in-time recovery.** There is no "restore to 14:32" story. The
  granularity is one backup.
* **No incremental backup.** Every backup is a full directory copy.
* **Moon only.** The Postgres and SQLite backends were deleted in 0.7.0;
  their durability stories left with them.
* **No replica-based backup.** Backing up a Moon replica instead of the master
  would remove the downtime requirement; untested here.

---

## 8. Re-running the drill

```bash
# build the two binaries the drill needs
( cd vendor/moon && cargo build --release --bin moon )
cargo build -p lunaris-memory --no-default-features --release \
  --example backup_restore_workload

# full drill (~1 min at 200 docs, ~4 min at 1000 — ingest dominates)
scripts/backup-restore-drill.sh --docs 200

# one leg, keeping the scratch dirs for inspection
scripts/backup-restore-drill.sh --only 2 --keep
```

The drill binds only ports **6395** and **6396**, refuses to run against
6379/6380/6381/6399, only stops Moon processes whose PID it spawned, never
issues `FLUSHALL`, and deletes its scratch directory on exit unless `--keep` is
passed. It exits non-zero if any equivalence assertion fails **or** if either
negative case stops losing data — the latter means Moon changed and this
document must be re-derived.
