# lunaris-storage-embedded

Zero-dependency embedded `StoragePort` backend for Lunaris, backed by SQLite.

- `Lunaris::open("memory://")` — in-process, ephemeral. The "just trying it"
  backend: no Docker, no Postgres, no Moon.
- `Lunaris::open("sqlite:///path/to/lunaris.db")` — file-backed, durable.

## Status (onboarding-overhaul phase 1, skeleton)

Implemented:

- bi-temporal MVCC KV (`atomic_write` / `read_as_of` / `scan_range`) — the
  atomic-write-once invariant holds (one SQLite transaction per `atomic_write`).
- vector / graph primitives are *persisted* by `atomic_write` (so no data is
  lost) but `vector_search` / `graph_traverse` still return
  `StorageError::NotSupported`.
- queue (`publish` / `subscribe` / `queue_depth`) and keyword (`KeywordPort`)
  return `StorageError::NotSupported`.

Pending: brute-force cosine `vector_search`, FTS5 `keyword_search`, an in-table
`SELECT … FOR UPDATE SKIP LOCKED`-style queue, and adjacency-table
`graph_traverse`. Once those land the `lunaris-conformance`
`run_storage_embedded` test is un-ignored.
