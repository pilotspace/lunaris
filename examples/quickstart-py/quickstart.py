"""Lunaris 10-minute quickstart — Python.

Mirrors `examples/quickstart-rs/src/main.rs` against the same single-shard
Moon. Demonstrates the v0.7 public Python surface:

    import lunaris
    from lunaris import EpisodeBuilder, Scope

    handle = await lunaris.open("moon://127.0.0.1:6380")
    scoped = handle.scoped(Scope("quickstart"))
    lsn    = await scoped.ingest(EpisodeBuilder("quickstart:demo", "..."))
    hits   = await scoped.recall("hello")

Moon is the only backend as of 0.7.0. If you are coming from 0.6.x see
`docs/migration/0.6-to-0.7.md`.

## Prerequisites

1. A single-shard Moon reachable on `moon://127.0.0.1:6380`. `--shards 1` is
   REQUIRED — Lunaris commits each ingest as one MULTI/EXEC transaction and a
   sharded Moon refuses cross-shard writes. README.md has the runbook, and is
   honest about why `docker pull` does not work yet.
2. `pip install lunaris`.
3. The granite-r2 Q4_K_M GGUF staged at `~/.lunaris/models/` for the default
   in-process llama.cpp embedder.
4. `export LUNARIS_STORE_URL="moon://127.0.0.1:6380"`.
5. `python quickstart.py`.

Expected output:

    quickstart: opening lunaris handle at moon://127.0.0.1:6380
    quickstart: ingested episode at lsn=<wall_ms>:<counter> under scope `quickstart`
    quickstart: recalled 1 hit(s) for 'hello'
    quickstart:   top hit score=0.83 text='# Hello from Lunaris ...'
"""

from __future__ import annotations

import asyncio
import os
import sys

import lunaris

# Imported at module scope on purpose. These two names ARE the public
# quickstart surface; binding them here means `python -c "import quickstart"`
# fails loudly the day either is renamed or dropped. That import is one of the
# gates `.github/workflows/examples.yml` runs — see examples/README.md.
from lunaris import EpisodeBuilder, Scope

SCOPE = "quickstart"
SOURCE = "quickstart:demo"
CONTENT = "# Hello from Lunaris\n\nThis is your first episode."
QUERY = "hello"


async def main() -> int:
    # No default on purpose: guessing `moon://127.0.0.1:6380` would let the
    # quickstart write demo episodes into whatever Moon happens to own that
    # port — which on a developer box is often a real store.
    store_url = os.environ.get("LUNARIS_STORE_URL")
    if not store_url:
        print(
            "error: set LUNARIS_STORE_URL=moon://127.0.0.1:6380 "
            "— see examples/quickstart-py/README.md",
            file=sys.stderr,
        )
        return 1

    print(f"quickstart: opening lunaris handle at {store_url}")
    handle = await lunaris.open(store_url)

    # `Scope` is a validating newtype (`^[A-Za-z0-9_\-.]{1,128}$`). It raises
    # ValueError rather than letting an unvalidated string reach the keyspace.
    scoped = handle.scoped(Scope(SCOPE))

    # The scope is stamped onto the episode by `ScopedLunaris.ingest`; the
    # builder carries no scope field, so a caller cannot smuggle one in.
    lsn = await scoped.ingest(EpisodeBuilder(SOURCE, CONTENT))
    print(f"quickstart: ingested episode at lsn={lsn} under scope `{SCOPE}`")

    # Recall through the scope-bound handle. Only hits from this scope's
    # partition come back; the retrieval root is Vector("chunks", 30).
    hits = await scoped.recall(QUERY)
    print(f"quickstart: recalled {len(hits)} hit(s) for {QUERY!r}")
    if hits:
        top = hits[0]
        # Trim the chunk body to one line so the demo output stays tidy.
        snippet = top["text"][:60]
        print(f"quickstart:   top hit score={top['score']:.2f} text={snippet!r}")

    # NOTE — there is deliberately no DSL section here. `scoped.dsl()` returns
    # the codegen-frozen native RetrievalBuilder, whose combinators raise
    # NotImplementedError and which exposes no `.execute()` at all. The DSL
    # builder that DOES work — `handle.recall().query(q).top(n).execute()` —
    # has no scope parameter, so it reads a different partition than the one
    # this script ingested into. The Rust quickstart shows the typed DSL form
    # end-to-end; see README.md ("Where the DSL stops").
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
