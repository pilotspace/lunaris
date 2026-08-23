"""Every CI `napi build` must carry the flags `package.json`'s build script sets.

F3: `crates/lunaris-ts/package.json` defines `scripts.build` as
`napi build --platform --release`, but NO CI job runs `npm run build`. All
three invocations call `npx napi build` directly, so the script is decorative
as far as CI is concerned — a flag added there is silently dropped by every
build, including `ts-prebuild.yml`, which produces the PUBLISHED artifact.

That is why F3 records "the CI invocation is the reason the obvious fix does
not work": the obvious fix for the generator defect is to pass napi a flag, and
the natural place to put it is the build script, where it would have no effect
and nothing would say so.

This guard does not fix the generator defect. It converts a silent bypass into
a failing test: add a flag to `scripts.build` and every CI site that does not
carry it is named. Wiring CI through `npm run build` instead is not available
as a fix — the three sites need different `--features`, which the single script
cannot express.
"""

from __future__ import annotations

import json
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[2]
PKG = ROOT / "crates" / "lunaris-ts" / "package.json"
WORKFLOWS = ROOT / ".github" / "workflows"


def build_script_flags() -> list[str]:
    """Flags `scripts.build` passes to `napi build`, e.g. ['--platform', '--release']."""
    script = json.loads(PKG.read_text())["scripts"]["build"]
    assert "napi build" in script, (
        f"scripts.build no longer invokes napi build ({script!r}); this guard is "
        "comparing against the wrong command."
    )
    tail = script.split("napi build", 1)[1]
    return [t for t in tail.split() if t.startswith("--")]


def ci_napi_invocations() -> list[tuple[str, int, str]]:
    """(workflow, line number, command) for every `napi build` CI runs.

    Two kinds of prose are excluded, both found by this guard failing on its
    own first run:

    * comment lines — a workflow's comments discuss the build flags, so a raw
      scan finds them in PROSE and lets a deleted flag pass (how the F29 guard
      shipped blind);
    * YAML key lines — the step TITLE
      `- name: npm install + napi build lunaris-ts WITH bindings-it` contains
      the phrase and no flags, so it reported as an offender missing every one.

    What is left is command text: the body of a `run:` block, or the command
    on a single-line `run: <cmd>`.

    `run:` must NOT be dropped as a key line. Excluding every `key: value` line
    was the second wrong cut here — it silently discarded
    `ts-prebuild.yml`'s inline `run: npx napi build ...`, which builds the
    PUBLISHED artifact and is the single most important site to check. The
    vacuity floor below is what caught it.
    """
    key_line = re.compile(r"^-?\s*[A-Za-z_-]+:\s")
    out: list[tuple[str, int, str]] = []
    for path in sorted(WORKFLOWS.glob("*.yml")):
        for n, line in enumerate(path.read_text().splitlines(), start=1):
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            if stripped.startswith("run:"):
                stripped = stripped[len("run:") :].strip()
            elif key_line.match(stripped):
                continue
            if stripped and re.search(r"\bnapi build\b", stripped):
                out.append((path.name, n, stripped))
    return out


def test_every_ci_napi_build_carries_the_build_scripts_flags() -> None:
    flags = build_script_flags()
    offenders = [
        (wf, n, cmd, f)
        for wf, n, cmd in ci_napi_invocations()
        for f in flags
        if f not in cmd
    ]
    assert not offenders, (
        "CI invokes `napi build` without flags that `crates/lunaris-ts/package.json`'s "
        "`scripts.build` sets. No CI job runs `npm run build`, so the script cannot "
        "supply them:\n"
        + "\n".join(f"  {wf}:{n} missing {f}\n    {cmd}" for wf, n, cmd, f in offenders)
        + "\n\nAdd the flag to each invocation above, or remove it from scripts.build "
        "if it was never meant to reach CI."
    )


def test_no_ci_job_relies_on_npm_run_build() -> None:
    """Pins the premise. If a job is ever wired through `npm run build`, this
    guard's whole reason to exist changes, and the comparison above becomes a
    duplicate of what npm already guarantees for that job."""
    via_npm = [
        (path.name, n, line.strip())
        for path in sorted(WORKFLOWS.glob("*.yml"))
        for n, line in enumerate(path.read_text().splitlines(), start=1)
        if not line.lstrip().startswith("#") and re.search(r"npm run build\b", line)
    ]
    assert not via_npm, (
        "a CI job now runs `npm run build`, which this guard assumes never happens:\n"
        + "\n".join(f"  {wf}:{n}  {cmd}" for wf, n, cmd in via_npm)
        + "\nRe-read F3: if CI goes through the script, flags propagate on their own "
        "and this file should be re-scoped to the jobs that still bypass it."
    )


def test_the_scanner_sees_the_invocations_it_is_meant_to_check() -> None:
    """Vacuity floor. Both tests above pass trivially when the scan finds
    nothing — an empty offender list and an empty invocation list are the same
    green. Pins that the flags and the call sites are both really there."""
    flags = build_script_flags()
    assert flags, (
        f"parsed no flags from scripts.build in {PKG}; the first test would then "
        "iterate an empty flag set and pass over any CI command at all."
    )
    calls = ci_napi_invocations()
    assert len(calls) >= 3, (
        f"expected at least 3 `napi build` invocations across {WORKFLOWS}; found "
        f"{len(calls)}: {calls}. Either the workflows changed shape or the scan "
        "stopped matching, in which case the first test is asserting nothing."
    )
    assert any(wf == "ts-prebuild.yml" for wf, _, _ in calls), (
        "ts-prebuild.yml builds the PUBLISHED artifact and must be among the "
        f"invocations checked; found only {sorted({wf for wf, _, _ in calls})}."
    )
