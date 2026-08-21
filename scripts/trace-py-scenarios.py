"""Print every input + output step through ChatAgentMemory + DocumentKnowledgeBase
against a live Moon. Not part of the evidence bundle — one-shot demo.
"""
import asyncio, json, os, sys
import lunaris
from lunaris.conversational import ChatAgentMemory
from lunaris.documentary import DocumentKnowledgeBase

MOON_URL = os.environ.get("LUNARIS_MOON_URL", "moon://127.0.0.1:6380")

def show(tag, payload):
    s = json.dumps(payload, indent=2, default=str) if not isinstance(payload, str) else payload
    print(f"\n── {tag} ──\n{s}")

async def main():
    print(f"# Backend\nMOON_URL = {MOON_URL}")

    show("STEP 1 · INPUT → lunaris.open()", {"url": MOON_URL})
    handle = await lunaris.open(MOON_URL)
    show("STEP 1 · OUTPUT", {"type": type(handle).__name__, "repr": repr(handle)[:100]})

    # ════════════ CHAT AGENT MEMORY ════════════
    print("\n\n════════════ SCENARIO: ChatAgentMemory ════════════")

    user_id = "demo-user-42"
    show("STEP 2 · INPUT → ChatAgentMemory(handle, user_id)", {"user_id": user_id})
    chat = ChatAgentMemory.new(handle, user_id)
    show("STEP 2 · OUTPUT", {"type": type(chat).__name__})

    turns = [
        "I'm building an agent memory engine called Lunaris.",
        "The milestone v0.1.1 targets sub-25ms recall.",
        "We use bi-temporal MVCC over Moon as the primary store.",
    ]
    recorded_lsns = []
    for i, t in enumerate(turns, 1):
        show(f"STEP 3.{i} · INPUT → chat.remember(turn)", {"turn": t})
        lsn = await chat.remember(t)
        recorded_lsns.append(lsn)
        show(f"STEP 3.{i} · OUTPUT (Moon LSN)", {"lsn": lsn})

    query = "Lunaris milestone"
    show("STEP 4 · INPUT → chat.recall(query)", {"query": query})
    hits = await chat.recall(query)
    show("STEP 4 · OUTPUT (Moon hits)", {"hit_count": len(hits), "hits": hits})

    # ════════════ DOCUMENT KNOWLEDGE BASE ════════════
    print("\n\n════════════ SCENARIO: DocumentKnowledgeBase ════════════")

    source_prefix = "demo-kb/"
    show("STEP 5 · INPUT → DocumentKnowledgeBase(handle, source_prefix)", {"source_prefix": source_prefix})
    kb = DocumentKnowledgeBase.new(handle, source_prefix)
    show("STEP 5 · OUTPUT", {"type": type(kb).__name__})

    chunks = [
        ("Lunaris uses bi-temporal MVCC over Moon for sub-25ms recall over millions of facts.",
         {"doc_id": "doc-001", "title": "architecture"}),
        ("Helios production hardening lands in Phase 12 of v0.1.1 with HeliosScratchpad v2.",
         {"doc_id": "doc-002", "title": "helios"}),
        ("The retrieval DSL fuses Vector (kNN), Keyword (BM25), and Graph (anchored) with RRF.",
         {"doc_id": "doc-003", "title": "dsl"}),
    ]
    show("STEP 6 · INPUT → kb.ingest([(body, meta) × 3])", {"chunks": [{"body": b[:60] + "…", "meta": m} for b, m in chunks]})
    result = await kb.ingest(chunks)
    show("STEP 6 · OUTPUT", {"result": result})

    query2 = "bi-temporal MVCC Moon recall"
    show("STEP 7 · INPUT → kb.top(3).search(query)", {"query": query2, "top_k": 3})
    hits2 = await kb.top(3).search(query2)
    show("STEP 7 · OUTPUT (fused RRF hits)", {"hit_count": len(hits2), "hits": hits2})

    print("\n\n✓ Done. All inputs + Moon outputs captured above.")

asyncio.run(main())
