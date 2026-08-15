//! The lossy contract — stated up front, never discovered afterwards.
//!
//! Every claim in [`LOSSY_CONTRACT`] is derived from a mechanism in this
//! repository, not from caution:
//!
//! 1. **Sys-time history collapses to migration time.**
//!    [`WriteOp`](lunaris_core::storage::WriteOp) carries no bi-temporal or LSN
//!    input — `KvPut { key, value }` is the entire write surface — and
//!    [`atomic_write`](lunaris_core::StoragePort::atomic_write) stamps the batch
//!    with the destination's own HLC tick. There is no backdating write on any
//!    backend, so the destination records "observed at migration time" for every
//!    row. The *record's own* `bt` field survives byte-for-byte inside the value
//!    payload; it is the storage-level sys interval that is re-stamped.
//!
//! 2. **Superseded versions do not travel.**
//!    The embedded backend keeps a real KV version chain (`lunaris_kv` is keyed
//!    `(key, sys_from)` and `close_open_kv` sets `sys_to` on overwrite); Postgres
//!    keeps the same shape. Moon KV is a plain single-field hash — `HSET key v
//!    <bytes>` — with no version chain at all, which is why
//!    `MoonStorage::supports_historical_kv_reads()` is `false`. Only the row that
//!    is current at migration time can be represented on the destination.
//!
//! 3. **Closed intervals are skipped, not flattened.**
//!    A row whose record-level validity is closed (`bt.valid.1 = Some(_)`) was
//!    retracted or superseded in the world; a row whose record-level sys interval
//!    is closed (`bt.sys.1 = Some(_)`) was logically deleted. Copying either one
//!    into a store that cannot express "was true until T" would present retracted
//!    state as current. They are counted and left behind.
//!
//! 4. **Derived indexes are not carried.**
//!    KV values omit embeddings on the wire (`#[serde(default,
//!    skip_serializing)]` on `Chunk::embedding` and friends, since `6093a9f`) and
//!    `StoragePort` exposes no vector enumeration — `vector_search` is a k-bounded
//!    ANN probe, not a dump. So the source's vectors are unreachable *through the
//!    port contract*, and Moon's FT documents live at a different key entirely
//!    (`lunaris_{scope}_{kind}_idx:{id_hex}`, written by `VectorUpsert`, never by
//!    `KvPut`). The same is true of the graph projection. Vector search, BM25
//!    keyword search, and graph traversal are all DEAD on the destination until a
//!    re-embed pass runs.
//!
//! 5. **What this tool cannot even count.**
//!    Superseded sys versions are invisible to a `StoragePort` reader:
//!    `scan_range(.., as_of = None)` returns the live row per key and nothing
//!    else, and a historical `as_of` is refused by the destination. The report
//!    therefore says `not enumerable` for that class instead of printing a zero
//!    that would read as "there were none".

/// The contract printed at startup, before any I/O.
///
/// Operators acknowledge it with `--acknowledge-lossy`; a run without that flag
/// can only report.
pub const LOSSY_CONTRACT: &str = "\
LOSSY MIGRATION CONTRACT — read before committing
-------------------------------------------------------------------------------
This tool performs a ONE-WAY, LOSSY copy of Lunaris primitives into Moon.

WHAT MIGRATES
  * Every KV primitive under lunaris:{scope}: that is CURRENT at read time and
    whose record-level intervals are both open (bt.valid.1 = null,
    bt.sys.1 = null), copied byte-for-byte as WriteOp::KvPut.

WHAT DOES NOT MIGRATE
  * Sys-time history. WriteOp carries no bi-temporal/LSN input and Moon has no
    backdating write, so the destination stamps every row with MIGRATION TIME.
    The record's own `bt` payload is preserved inside the value; the storage
    sys interval is re-stamped. Bi-temporal AS-OF reads over pre-migration
    history are NOT reconstructible on Moon.
  * Superseded versions. Moon KV is a single-field hash with no version chain
    (supports_historical_kv_reads() == false). Only the current row can exist.
  * Closed intervals. Rows with bt.valid.1 set (retracted / superseded in the
    world) or bt.sys.1 set (logically deleted) are SKIPPED and counted —
    copying them would present retracted state as current.
  * Derived indexes. Vector embeddings, the BM25/FT documents, and the graph
    projection are NOT carried: KV values omit embeddings on the wire and the
    StoragePort exposes no vector enumeration. RECALL WILL RETURN NOTHING on
    the destination until a re-embed / re-index pass runs. Use
    --reembed-manifest to emit the exact key list that needs regeneration.

WHAT CANNOT BE COUNTED
  * The number of superseded sys versions left behind. A StoragePort reader
    sees one live row per key; the destination refuses historical reads. The
    report prints `not enumerable` rather than a misleading zero.

RE-RUN SEMANTICS
  Keys are deterministic (lunaris:{scope}:{kind}:{ulid}) and every write is an
  idempotent KvPut, so re-running overwrites in place and never duplicates.
  A re-run after new source writes is a valid incremental top-up.
-------------------------------------------------------------------------------";

/// The one-line refusal shown when a commit is attempted without acknowledgement.
pub const ACK_REQUIRED: &str =
    "refusing to write: re-run with --acknowledge-lossy once the contract above is understood";

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract is the product here — a silent edit that drops a clause is
    /// exactly the "discovered as a surprise" failure this task exists to
    /// prevent. Pin the load-bearing claims.
    #[test]
    fn contract_states_every_lossy_class() {
        for claim in [
            "MIGRATION TIME",
            "Superseded versions",
            "Closed intervals",
            "Derived indexes",
            "not enumerable",
            "--reembed-manifest",
        ] {
            assert!(LOSSY_CONTRACT.contains(claim), "contract lost the {claim:?} clause");
        }
    }

    #[test]
    fn ack_message_names_the_flag() {
        assert!(ACK_REQUIRED.contains("--acknowledge-lossy"));
    }
}
