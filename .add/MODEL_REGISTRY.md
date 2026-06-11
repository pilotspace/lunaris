# MODEL_REGISTRY  (which AI wrote this project — for reproducibility & audit)

Model: Claude (Fable 5, claude-fable-5) via Claude Code CLI
Version: 2026-06 (Fable 5); earlier waves authored with Opus/Sonnet 4.x via GSD agents (see .planning/ history)
Adopted: 2026-06-11
Notes: re-run the playbook golden-cases before changing this.
      Pre-ADD history: ~all of crates/* is AI-authored under the GSD workflow
      (gsd-planner/gsd-executor agents, .planning/ submodule holds the audit trail).
      Runtime inference models INSIDE the product (granite-embedding-311m, bge-reranker-v2-m3)
      are documented in root CLAUDE.md §Technology Stack, not here.
