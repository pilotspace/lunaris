//! `lunaris-migrate` — the operator exit ramp from the SQLite / Postgres
//! backends to Moon, built BEFORE those backends are deleted.
//!
//! 0.7.0 removes `lunaris-storage-postgres` and `lunaris-storage-embedded`.
//! Anyone running on either one needs a way out that states its losses as a
//! contract instead of letting them be discovered later. That is this crate.
//!
//! ```text
//! lunaris-migrate --from sqlite:///var/lib/lunaris/store.db \
//!                 --to   moon://127.0.0.1:6379 \
//!                 --all-scopes                    # report only (default)
//!
//! lunaris-migrate --from ... --to ... --all-scopes --commit --acknowledge-lossy
//! ```
//!
//! # Design in one paragraph
//!
//! Reads go through the EXISTING [`StoragePort`](lunaris_core::StoragePort)
//! implementations — no backend private API, no second copy of anyone's SQL. One
//! `scan_range` per scope over the `lunaris:{scope}:` prefix yields the current
//! row per key; each row is classified ([`plan::classify_row`]); survivors are
//! re-written as `WriteOp::KvPut` batches through
//! [`atomic_write`](lunaris_core::StoragePort::atomic_write). Because the
//! destination handle is a normal `MoonStorage::connect`, the connect-time
//! multi-shard guard and server-version handshake apply unchanged — this tool
//! deliberately has no way to bypass them.
//!
//! # The lossy contract
//!
//! See [`contract::LOSSY_CONTRACT`], printed before any I/O and gated behind
//! `--acknowledge-lossy`. The short version: only CURRENT, open-interval rows
//! migrate; sys-time history collapses to migration time; closed/superseded
//! intervals are counted and left behind; vector, BM25 and graph state are
//! derived indexes that this tool does not rebuild.
//!
//! # Embeddings — what was investigated, and what is honest
//!
//! Three facts close this question:
//!
//! * KV payloads carry no vectors. `Chunk`/`Entity`/`Fact`/`Community` mark
//!   `embedding` `#[serde(default, skip_serializing)]` (since `6093a9f`), so the
//!   bytes `scan_range` returns cannot contain one.
//! * `StoragePort` offers no vector enumeration. `vector_search` is a k-bounded
//!   ANN probe against an index name; there is no "dump every vector" method on
//!   the trait, so the source's vectors are unreachable through the contract
//!   this tool is allowed to use.
//! * On Moon, FT documents are separate keys — `VectorUpsert` writes
//!   `lunaris_{scope}_{kind}_idx:{id_hex}`, while `KvPut` writes
//!   `lunaris:{scope}:{kind}:{ulid}`. Copying KV therefore creates no FT
//!   document, and Moon's BM25 path reads the same FT index as the vector path.
//!
//! Re-embedding inside this tool would mean linking the llama.cpp runtime into a
//! migration binary and silently re-deriving a model-version-dependent artifact
//! during a data move — a worse failure mode than an honest gap. So the choice
//! is: **carry the durable record, declare the derived state, and hand the
//! operator the exact backlog**. `--reembed-manifest <path>` writes one JSONL
//! line per key that needs a vector regenerated. Until that pass runs, recall on
//! the destination returns nothing.
//!
//! # How this tool survives 0.7.0
//!
//! It does not, and that is the design. `lunaris-migrate` is pinned to the
//! 0.6.x line — the last one where all three backends compile in a single
//! workspace — and is deleted alongside `lunaris-storage-postgres` and
//! `lunaris-storage-embedded` in 0.7.0. The alternatives were worse:
//!
//! | Option | Why not |
//! |---|---|
//! | Pin published crates.io versions of the storage crates | Two `lunaris-core` versions in one binary; `StoragePort` from 0.6 and 0.7 are different traits, so the tool would need a hand-written bridge that nobody exercises. And `lunaris-storage-moon` depends on `moondb` by PATH, so it is not on crates.io to pin. |
//! | Keep the source backends in-workspace as migrate-only deps | Slice B's deletion becomes cosmetic: the code still has to compile, and therefore still has to be maintained, against every 0.7.x change. |
//! | **Run migration on the 0.6.2 release binary** (chosen) | Zero ongoing maintenance. The tool is frozen at the last version where both sides provably exist, which is also the only version where its behaviour can be tested end to end. |
//!
//! Operator procedure: run `lunaris-migrate` from the 0.6.2 release, verify,
//! then upgrade the server to 0.7.0. See `docs/migration/0.6-to-0.7.md`.

#![forbid(unsafe_code)]

pub mod contract;
pub mod migrate;
pub mod plan;
pub mod verify;

pub use contract::{ACK_REQUIRED, LOSSY_CONTRACT};
pub use migrate::{MigrateError, ScopeReport, discover_scopes, migrate_scope};
pub use plan::{
    DEFAULT_BATCH_SIZE, DEFAULT_SAMPLE, MigrationOptions, RowVerdict, classify_row, kind_of,
    needs_reembed,
};
pub use verify::{VerifyReport, verify_scope};
