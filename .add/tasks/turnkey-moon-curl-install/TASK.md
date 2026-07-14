# TASK: Turnkey requires curl-installed Moon (Moon-only)

slug: turnkey-moon-curl-install · created: 2026-07-14 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `scripts/setup-lunaris-agents.py:33` — `MOON_BIN = ROOT/"vendor"/"moon"/"target"/"release"/"moon"` (vendored build artifact; today the ONLY resolution path)
- `scripts/setup-lunaris-agents.py:112-117` — `--storage-backend choices=("moon","sqlite") default="moon"` (sqlite must now be rejected)
- `scripts/setup-lunaris-agents.py:130-142` — `--build-moon` (BooleanOptionalAction, default False) + `--moon-bin` (default `str(MOON_BIN)`)
- `scripts/setup-lunaris-agents.py:176-177,197-198` — build-moon flow → `ensure_moon_prereqs` (cargo build vendored) + success print
- `scripts/setup-lunaris-agents.py:620-659` — `ensure_moon_running(args, storage_url)` — autostart gates on `Path(args.moon_bin).exists()`; failure recipe names `args.moon_bin`
- `scripts/setup-lunaris-agents.py:287-292` — `effective_storage_url` (sqlite → None)
- `docs/integration/claude-code.md:15-30,54-114` — turnkey lead + Quick Start currently lead with `--build-moon`
- `README.md:74` — "`--storage-backend sqlite` to opt out" (now stale)
- Moon install source of truth: `vendor/moon/README.md:86` — `curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh | sh` → installs to `~/.local/bin` (pin: `VERSION=v0.6.0 INSTALL_DIR=/usr/local/bin sh install.sh`)

Context (working folder): tests in `scripts/tests/` (pytest; `test_turnkey_verify.py:117` mentions --build-moon in a doc-string echo only)
Honors (patterns / conventions): user directives 2026-07-14 — "instead --build-moon, we will required and suggest to install Moon via CURL as guide in Moon repo readme.md" + "lunaris just support Moon db only". Memory `project_moon_only_direction` (full PG/SQLite deletion is a future milestone; turnkey rejection is the first user-visible cut). Red/green TDD; tmp/ commit-message file.
Anchors the contract cites: `resolve_moon_bin` (new), `MOON_CURL_INSTALL` (new const), `ensure_moon_running`, `--storage-backend`, `--build-moon`, `--moon-bin`

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: turnkey Moon-binary resolution via curl-installed Moon; Moon-only backend
Framings weighed: resolve-PATH-first-fail-with-curl-hint (chosen) · keep --build-moon primary with curl as docs-only note · delete sqlite choice from argparse (loses the guidance message)
Must:
<must>
  - New resolution order for the Moon binary: (1) explicit `--moon-bin` if provided, (2) `moon` on PATH — including `~/.local/bin` even when not on PATH (curl install target), (3) vendored `vendor/moon/target/release/moon` if present.
  - When no binary resolves AND the flow needs one (setup with moon backend, or --verify autostart), fail/warn with the exact curl one-liner from vendor/moon/README.md.
  - `--build-moon` still works but prints a deprecation warning pointing at the curl install.
  - `ensure_moon_running` uses the resolved binary (not raw `args.moon_bin`) for both the autostart Popen and the failure recipe text.
  - `--storage-backend sqlite` is rejected at startup with a Moon-only message that includes the curl one-liner.
  - docs/integration/claude-code.md turnkey lead becomes: step 0 = curl-install Moon, then the two commands WITHOUT --build-moon; README.md:74 sqlite opt-out line updated.
</must>
Reject:
<reject>
  - `--storage-backend sqlite` -> exit 2, message contains "Moon-only" + curl one-liner
  - explicit `--moon-bin <nonexistent>` -> exit 2, message contains the path + curl one-liner (explicit path is trusted, never silently fallen through)
  - no binary anywhere (setup, moon backend) -> exit 2, message contains curl one-liner
</reject>
After:
<after>
  - A user on a fresh machine runs the curl install, then the two turnkey commands with zero cargo-build of Moon; setup resolves `~/.local/bin/moon` automatically.
  - `--build-moon` path unchanged functionally (dev fallback) but marked deprecated.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The curl installer's target `~/.local/bin` may not be on PATH in hook/CI contexts — lowest confidence because PATH varies per shell; if wrong: resolution silently misses the installed binary. Mitigation: probe `~/.local/bin/moon` explicitly, not just `shutil.which`.
  - [x] Explicit `--moon-bin` should ERROR when missing rather than fall through — confirmed by "required" in the user directive; silent fallback would mask typos.
  - [x] sqlite rejection belongs in main() (not argparse choices removal) so the error can carry the curl guidance — confirmed: argparse invalid-choice can't carry the hint.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: sqlite backend rejected as Moon-only
  Given the turnkey script
  When run with --storage-backend sqlite --dry-run
  Then it exits non-zero with a message containing "Moon-only" and the curl install one-liner
  And no config file is written

Scenario: explicit missing --moon-bin errors with curl hint
  Given no file at /nonexistent/moon
  When run with --moon-bin /nonexistent/moon --dry-run
  Then it exits non-zero with a message naming the path and the curl one-liner
  And no config file is written

Scenario: PATH-installed moon resolves without --build-moon
  Given a fake `moon` executable on PATH (or at ~/.local/bin)
  When resolve_moon_bin runs with default args
  Then it returns that executable's path

Scenario: vendored binary is the last fallback
  Given no moon on PATH and no explicit --moon-bin
  When resolve_moon_bin runs and vendor/moon/target/release/moon exists
  Then it returns the vendored path

Scenario: --build-moon prints a deprecation warning
  Given the turnkey script
  When run with --build-moon --dry-run
  Then output contains a deprecation notice mentioning the curl install
  And the vendored build still executes (dry-run prints the cargo command)
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
scripts/setup-lunaris-agents.py

MOON_CURL_INSTALL: str  = "curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh | sh"

resolve_moon_bin(args) -> Path | None
  # order: explicit --moon-bin (error if missing — SystemExit msg contains path + MOON_CURL_INSTALL)
  #        shutil.which("moon") → Path
  #        ~/.local/bin/moon if exists → Path
  #        MOON_BIN (vendored) if exists → Path
  #        else None
  # "explicit" = args.moon_bin != str(MOON_BIN) (argparse default sentinel)

main():
  --storage-backend sqlite -> SystemExit(2) msg contains "Moon-only" + MOON_CURL_INSTALL
  --build-moon             -> stderr deprecation line mentioning curl install; behavior kept
  setup (moon backend) with resolve_moon_bin()==None -> SystemExit(2) msg contains MOON_CURL_INSTALL
  success print names the resolved binary path

ensure_moon_running(args, storage_url):
  uses resolve_moon_bin(args) for autostart + recipe text; None -> failure detail contains MOON_CURL_INSTALL

docs: docs/integration/claude-code.md turnkey = 3 steps (curl, setup, verify); README.md:74 drops sqlite opt-out.
```

Status: FROZEN @ v1 — approved by standing fully-auto delegation (Tin Dang, this session's explicit directives).
Least-sure flag surfaced at freeze: [contract] treating `args.moon_bin != str(MOON_BIN)` as the "explicitly provided" sentinel — a user passing the vendored path verbatim gets fallback semantics instead of strict; cost: soft-fail instead of hard error in one edge case, acceptable (same resolution result when the file exists).

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every Must + Reject has one discriminating test
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_sqlite_backend_rejected_moon_only: run script --storage-backend sqlite --dry-run → rc!=0, output has "Moon-only" + "curl -fsSL"
  - test_explicit_missing_moon_bin_errors_with_curl_hint: --moon-bin /nonexistent/moon --dry-run → rc!=0, output names path + "curl -fsSL"
  - test_path_installed_moon_resolves: fake moon executable in tmp dir prepended to PATH → resolve_moon_bin returns it
  - test_vendored_binary_is_last_fallback: PATH stripped, HOME redirected (no ~/.local/bin/moon), monkeypatched MOON_BIN exists → resolved
  - test_build_moon_prints_deprecation: --build-moon --dry-run → rc==0, output mentions deprecat* + curl
</test_plan>

Tests live in: `scripts/tests/test_turnkey_moon_only.py` · MUST run red (missing implementation) before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `scripts/setup-lunaris-agents.py` `scripts/tests/test_turnkey_moon_only.py` `scripts/tests/test_turnkey_verify.py` `docs/integration/claude-code.md` `docs/../README.md`
Change request note: `test_turnkey_verify.py::test_docs_lead_with_two_commands` pins the OLD command 1 (`--build-moon`) from the claude-code-turnkey contract; the 2026-07-14 user directive supersedes it — that one assertion is amended to the new curl-first command as part of this task (not a green-forcing edit).
Strategy (ordered batches): 1. resolve_moon_bin + MOON_CURL_INSTALL + sqlite rejection + deprecation warning in script  2. wire ensure_moon_running + success prints  3. docs sweep
Safety rule (feature-specific): never delete --build-moon behavior (dev fallback); explicit --moon-bin never silently falls through.
Constraints: do NOT change any test or the contract; no new dependencies.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — test_turnkey_moon_only.py 9/9; test_turnkey_verify.py 4/4 (1 live-gated skip); test_contextd_lifecycle.py 4/4 (1 live-gated skip)
- [x] coverage did not decrease — 9 new tests added, none removed
- [x] no test or contract was altered during build — one declared change-request amendment (test_docs_lead_with_two_commands, superseded old claude-code-turnkey pin per 2026-07-14 user directive; declared in §5 BEFORE build)
- [x] the green was EARNED — red-first 9/9 (5 failures + 4 AttributeError on missing resolve_moon_bin); subprocess-level tests assert real exit codes/messages, resolver unit tests use real files
- [x] concurrency / timing safe — resolver is pure path probing; autostart Popen unchanged semantics (binary path now resolved, gate condition equivalent)
- [x] no exposed secrets / injection / new deps — stdlib only; curl one-liner is a printed suggestion, never executed by the script
- [x] layering follows conventions — script-local helper, no cross-crate impact
- [x] reviewed — self-review under standing fully-auto delegation (diff read in full)

### Build expectations — confirmed at the gate
- [x] `--storage-backend sqlite --dry-run` exits 2 printing the curl one-liner — ran it: "Lunaris is Moon-only… curl -fsSL …" rc=2
- [x] plain `--dry-run` on this box resolves and succeeds — ran it: "- Moon: using …/vendor/moon/target/release/moon", rc=0
- [x] existing suites still green — all three scripts/tests files OK
- [x] docs turnkey block: curl step 0 first, commands 1–2 without --build-moon — read post-edit
- [x] BONUS live proof: full two-command turnkey (temp settings, --moon-url moon://127.0.0.1:6381) → VERIFY PASS capture + VERIFY PASS cross-session inject

### Deep checks
- [x] WIRING — resolve_moon_bin called from main() setup path (missing-binary gate + success print) AND ensure_moon_running (autostart + recipe); live verify exercised the ensure_moon_running path end-to-end
- [x] DEAD-CODE — remaining raw MOON_BIN/args.moon_bin uses are the resolver itself, ensure_moon_prereqs (build target check, correct), and the deprecated build print; no resolution bypass
- [x] SEMANTIC — claude-code.md turnkey + Quick Start + storage sections and README.md §Quick Start read in full; no remaining "--build-moon" as a recommended path, no sqlite opt-out claim

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (auto-gate, evidence above; standing fully-auto delegation from Tin Dang) · date: 2026-07-14

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): setup failure rate naming the curl hint; verify `storage` stage failures

### Spec delta

### Competency deltas
