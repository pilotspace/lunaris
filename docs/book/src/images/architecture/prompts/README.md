# Architecture diagram prompts

The diagrams in `../` are AI-generated (Gemini
`gemini-3-pro-image-preview`, 16:9, 2K) in the "calm whiteboard" style
with two deliberate overrides: clean **print lettering** (not
handwriting) and **pencil-line annotation arrows**.

To regenerate after a content change, edit the matching prompt file and
run (requires `GEMINI_API_KEY`):

```bash
python3 nanobanana_rest.py \
  --prompt "$(cat diagram-layers.txt)" \
  --aspect-ratio 16:9 --image-size 2K \
  --output ../lunaris-layers.png
```

| Prompt | Output | Shows |
|---|---|---|
| `diagram-layers.txt` | `lunaris-layers.png` | The four-tier layered design |
| `diagram-pipeline.txt` | `lunaris-pipeline.png` | Ingest → atomic write → Moon → recall fusion |
| `diagram-stack.txt` | `moon-vs-stack.png` | Four-system stack vs one Moon substrate |
| `diagram-feature-superpower.txt` | `moon-feature-superpower.png` | Feature → Moon native command → reader benefit |
| `diagram-compare-rivals.txt` | `lunaris-vs-rivals.png` | Lunaris vs Mem0 / Zep / Cognee scorecard (cells from the why-lunaris table) |
| `diagram-mcp-flow.txt` | `lunaris-mcp-flow.png` | AI client → MCP tools (RETRIEVE/STORE/MANAGE) → engine → store, with the progressive-disclosure retrieval ladder |
| `diagram-hook-flow.txt` | `lunaris-hook-flow.png` | lunaris-hook capture pipeline + lunaris-contextd inject loop over the shared scope store |
| `diagram-scratchpad-agents.txt` | `lunaris-scratchpad-agents.png` | Multi-agent shared scratchpad: scope blackboard, per-agent namespaces, ACT-R consolidation into durable memory |
| `diagram-resume-ladder.txt` | `lunaris-resume-ladder.png` | Agent resumes mid-session: 3-tier progressive-disclosure retrieval ladder (scratchpad → recall k=5 → widen + bi-temporal store) |
| `diagram-data-01-observation-episode.txt` | `lunaris-data-01-observation-episode.png` | Episode envelope: id, scope, source, content, t_ref, bt |
| `diagram-data-02-chunking-doctree.txt` | `lunaris-data-02-chunking-doctree.png` | Chunk fields + DocTree node-edge structure |
| `diagram-data-03-embedding-raptor.txt` | `lunaris-data-03-embedding-raptor.png` | granite 768-d embedding + RAPTOR Community tree |
| `diagram-data-04-graph-primitives.txt` | `lunaris-data-04-graph-primitives.png` | Entity, Relation, Fact (opt-in / structured ingest) |
| `diagram-data-05-atomic-persist.txt` | `lunaris-data-05-atomic-persist.png` | ONE Vec<WriteOp> → one atomic_write (INGEST-04) |
| `diagram-data-06-keyspace.txt` | `lunaris-data-06-keyspace.png` | Canonical key format lunaris:{scope}:{kind}:{ulid} |
| `diagram-data-07-bitemporal-mvcc.txt` | `lunaris-data-07-bitemporal-mvcc.png` | valid-time vs system-time MVCC grid + read_as_of |
| `diagram-data-08-indexes.txt` | `lunaris-data-08-indexes.png` | Vector FT / BM25 / graph edges → RRF fusion |

Keep prompts sparse: image models garble dense technical text. Numbers
quoted in the prompts (recall p50 ≈ 19–22 ms at 100k documents per
scope, contract < 25 ms) must stay in sync with
[`docs/operations/capacity.md`](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md) —
update both or neither. Always inspect the generated PNG for spelling
before committing.

> **Regeneration owed (2026-08-21).** The prompt sources were updated to
> the GA-2b envelope when the `p50 10.3 ms` figure was retracted repo-wide,
> but the **committed PNGs still render the retracted number** — image
> regeneration needs a model run and was out of scope for the retraction
> sweep. `lunaris-layers.png`, `lunaris-pipeline.png` and
> `lunaris-stack.png` must be regenerated from these prompts before the
> next release that ships the book or the README hero image.
