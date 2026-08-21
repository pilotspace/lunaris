"""Verify whether adding a brief index-flush wait + retries produces non-zero
recall/search hits against a live Moon with a real Ollama embedder."""
import asyncio, json, os, time
import lunaris
from lunaris.conversational import ChatAgentMemory
from lunaris.documentary import DocumentKnowledgeBase

MOON_URL = os.environ.get("LUNARIS_MOON_URL", "moon://127.0.0.1:6380")

def show(tag, p):
    print(f"\n── {tag} ──\n{json.dumps(p, indent=2, default=str) if not isinstance(p, str) else p}")

async def wait_for_hits(fn, deadline_s=10.0, step_s=0.5):
    """Poll `fn()` until it returns a non-empty list or the deadline elapses."""
    t0 = time.perf_counter()
    attempt = 0
    while True:
        attempt += 1
        hits = await fn()
        elapsed = time.perf_counter() - t0
        print(f"  [attempt {attempt:>2}  elapsed {elapsed*1000:.0f}ms] hits={len(hits)}")
        if len(hits) > 0:
            return hits, elapsed, attempt
        if elapsed >= deadline_s:
            return hits, elapsed, attempt
        await asyncio.sleep(step_s)

async def main():
    print(f"# Backend {MOON_URL}  # Ollama http://localhost:11434 (embeddinggemma:300m)")

    handle = await lunaris.open(MOON_URL)

    # ── ChatAgentMemory ──
    print("\n════════════ ChatAgentMemory ════════════")
    chat = ChatAgentMemory.new(handle, "demo-user-42")

    turns = [
        "I'm building an agent memory engine called Lunaris.",
        "The milestone v0.1.1 targets sub-25ms recall.",
        "We use bi-temporal MVCC over Moon as the primary store.",
    ]
    for t in turns:
        lsn = await chat.remember(t)
        show(f"remember", {"input": t, "lsn": lsn})

    print("\n── chat.recall('Lunaris milestone') — polling for index flush ──")
    hits, elapsed, attempts = await wait_for_hits(lambda: chat.recall("Lunaris milestone"))
    show("FINAL chat.recall OUTPUT", {
        "elapsed_s": round(elapsed, 2), "attempts": attempts,
        "hit_count": len(hits),
        "hits": hits[:3] if hits else "NONE — index may need >10s warm-up or query tuning",
    })

    # ── DocumentKnowledgeBase ──
    print("\n\n════════════ DocumentKnowledgeBase ════════════")
    kb = DocumentKnowledgeBase.new(handle, "demo-kb/")
    chunks = [
        ("Lunaris uses bi-temporal MVCC over Moon for sub-25ms recall over millions of facts.",
         {"doc_id": "doc-001", "title": "architecture"}),
        ("Helios production hardening lands in Phase 12 of v0.1.1 with HeliosScratchpad v2.",
         {"doc_id": "doc-002", "title": "helios"}),
        ("The retrieval DSL fuses Vector (kNN), Keyword (BM25), and Graph (anchored) with RRF.",
         {"doc_id": "doc-003", "title": "dsl"}),
    ]
    await kb.ingest(chunks)
    show("ingest OUTPUT", {"ingested_chunks": len(chunks), "preview": [c[0][:50] + "…" for c in chunks]})

    print("\n── kb.top(3).search('bi-temporal MVCC') — polling ──")
    hits2, elapsed2, attempts2 = await wait_for_hits(lambda: kb.top(3).search("bi-temporal MVCC"))
    show("FINAL kb.search OUTPUT", {
        "elapsed_s": round(elapsed2, 2), "attempts": attempts2,
        "hit_count": len(hits2),
        "hits": hits2[:3] if hits2 else "NONE",
    })

asyncio.run(main())
