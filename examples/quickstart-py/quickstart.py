"""Lunaris 10-minute quickstart — Python.

Mirrors `examples/quickstart-rs/src/main.rs` against the same Postgres
backend. Demonstrates the v0.2.x public Python surface:

    import lunaris
    handle = await lunaris.open("postgres://...")
    lsn = await handle.ingest(episode_dict)
    hits = await handle.recall().execute()

## Prerequisites

1. `docker compose -f ../quickstart-rs/docker-compose.yml up -d` —
   Postgres 16 + pgvector + pgmq + AGE on `localhost:5432` (reuse the
   Rust quickstart's image).
2. `sqlx migrate run --source ../../crates/lunaris-storage-postgres/migrations
      --database-url postgres://lunaris:lunaris@localhost:5432/lunaris`.
3. `ollama serve &` and `ollama pull nomic-embed-text`.
4. `pip install lunaris python-ulid` (or `uv add lunaris python-ulid`).
5. `export LUNARIS_PG_URL="postgres://lunaris:lunaris@localhost:5432/lunaris"`.
6. `python quickstart.py`.

Expected output:

    quickstart: opening lunaris handle at postgres://...
    quickstart: ingested episode at lsn=<wall_ms>:<counter> under scope `quickstart`
    quickstart: ingest path verified; see README for recall walkthrough
"""

from __future__ import annotations

import asyncio
import os
import sys

import lunaris
import ulid


def _build_episode(scope: str, content: str) -> dict:
    """Construct a dict shaped like `lunaris_core::primitives::Episode`.

    The Python surface accepts dicts on the wire; the v0.2 `scope`
    field is required. Production callers should reach for the
    `Scope` newtype + `EpisodeBuilder` once the typed Python surface
    lands in v0.3.
    """
    ep_id = ulid.ULID()
    return {
        "id": str(ep_id),
        "scope": scope,
        "source": "quickstart:demo",
        "content": content,
        "t_ref": None,
        "bt": {
            "valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
            "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
        },
        "metadata": {},
    }


async def main() -> int:
    pg_url = os.environ.get("LUNARIS_PG_URL")
    if not pg_url:
        print("error: set LUNARIS_PG_URL — see examples/quickstart-py/README.md")
        return 1

    print(f"quickstart: opening lunaris handle at {pg_url}")
    handle = await lunaris.open(pg_url)

    scope = "quickstart"
    episode = _build_episode(scope, "# Hello from Lunaris\n\nFirst Python ingest.")
    lsn = await handle.ingest(episode)
    print(f"quickstart: ingested episode at lsn={lsn} under scope `{scope}`")
    print("quickstart: ingest path verified; see README for recall walkthrough")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
