<!-- ADD:BEGIN — managed by `add.py sync-guidelines`; do not edit inside -->
## ADD — how to work in this repo

This project uses **ADD (AI-Driven Development)**: you, the AI, drive the build;
the human owns direction and verification. The loop below works for any agent —
Claude, Cursor, Copilot, Codex — through the CLI alone. Before you change code:

1. Run `python3 .add/tooling/add.py status` — where the project is and what's
   next (the resume point; read it first every session).
2. Read `.add/PROJECT.md` — the foundation (domain · spec · UI/UX) every task
   builds on.
3. Run `python3 .add/tooling/add.py guide` — it names the phase and the exact
   phase-guide file to read (the `guide  :` line). Work ONLY that phase — each
   guide ends with its exit gate and the command to move on.

The flow: INTAKE sizes a request into a milestone; each task runs the
**specification bundle** — Spec+Scenarios+Contract+Tests as one bundle,
ONE human approval at the frozen contract — then a self-driving build→verify
run. Non-negotiable for every agent:
Never weaken a test or edit a frozen contract to make a build pass; a security
finding is always HARD-STOP — never auto-passed.

On Claude Code the `add` skill drives this loop automatically; other agents
follow the three steps. The book is in `.add/docs/`. This block is generated
by `add.py sync-guidelines`; edit outside the markers, not inside.
<!-- ADD:END -->
