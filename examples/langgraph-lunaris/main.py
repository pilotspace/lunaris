"""LangGraph + Lunaris — store/recall agent memory through LunarisStore.

LIVE example: requires a running lunaris-server (which needs a single-shard
Moon behind it). The wire credential is an OPAQUE BEARER TOKEN, not a JWT — the
server maps it to {tenant, scopes} from its --tokens-file, and the partition
scope is the `tenant` claim, resolved server-side.

    export LUNARIS_SERVER_URL=http://127.0.0.1:8080
    export LUNARIS_TOKEN=demo-token
    python main.py

See README.md for the full runbook, including why Moon v0.8.5 has to be built
from source right now.
"""
from __future__ import annotations

import asyncio
import os

from lunaris_integrations import HttpLunarisClient
from lunaris_integrations.langgraph import LunarisStore


def build_store() -> LunarisStore:
    base_url = os.environ["LUNARIS_SERVER_URL"]
    token = os.environ["LUNARIS_TOKEN"]
    # The factory builds a scope-bound client per namespace. Swap in
    # SdkLunarisClient(handle, scope) to use the in-process wheel instead.
    return LunarisStore(
        lambda scope: HttpLunarisClient(base_url=base_url, token=token, scope=scope)
    )


async def main() -> None:
    store = build_store()
    namespace = ("user-42", "memories")

    await store.aput(namespace, "m1", {"text": "Alice joined Acme in 2021"})
    await store.aput(namespace, "m2", {"text": "Acme is headquartered in Berlin"})

    results = await store.asearch(namespace, "where is Acme based", limit=3)
    for item in results:
        print(f"{item.score:.3f}  {item.value['content']}")


if __name__ == "__main__":
    asyncio.run(main())
