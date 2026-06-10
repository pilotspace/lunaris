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

Keep prompts sparse: image models garble dense technical text. Numbers
quoted in the prompts (recall p50 10.3 ms, contract < 25 ms) must stay
in sync with `docs/benchmarks/v0.2.x/README.md` — update both or
neither. Always inspect the generated PNG for spelling before
committing.
