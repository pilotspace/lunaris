#!/usr/bin/env python3
"""Executable gate for docs/competitive/mem0-gap-analysis.md (TASK mem0-gap-analysis, §3 CONTRACT v1).

Parses the frozen §A–§F structure and asserts every §1 Must, emitting exactly the §1 Reject
codes on failure:  unsourced_claim · unwired_claim · dangling_p0 · apples_to_oranges · incomplete_coverage

Structural proxies (the deeper semantic "is it really wired / is the source current" is the §6
adversarial refute-read; this gate enforces the falsifiable structural floor):
  - at-parity rows must cite a code anchor that resolves to a real NON-test file (production proxy)
  - mem0_source must contain a URL and an ISO date
  - §D must carry a methodology marker whenever it quotes a number
Usage: python3 validate_gap_analysis.py [path-to-doc]   (exit 0 = green; prints codes + exit 1 on fail)
"""
from __future__ import annotations
import re
import sys
from pathlib import Path

# repo root: tests/ -> mem0-gap-analysis/ -> tasks/ -> .add/ -> <root>
REPO_ROOT = Path(__file__).resolve().parents[4]
DEFAULT_DOC = REPO_ROOT / "docs" / "competitive" / "mem0-gap-analysis.md"

REQUIRED_DIMENSIONS = {
    "reliability", "eval", "observability", "correctness-security",
    "memory-update-intelligence", "multi-level-memory", "graph-quality", "sdk-dx",
}
VERDICTS = {"ahead", "at-parity", "partial(built-not-wired)", "gap-missing"}
SEVERITIES = {"P0", "P1", "P2"}
METHODOLOGY_MARKERS = (
    "methodology", "like-for-like", "strict-replay", "noop", "historical",
    "not-run", "flagged", "retrieval-only", "embed-out-of-loop",
)
NUMBER_RE = re.compile(r"(\b\d+(?:\.\d+)?\s?(?:ms|s|%)\b|recall@\d+|\bp(?:50|95|99)\b)", re.I)
URL_RE = re.compile(r"https?://\S+")
DATE_RE = re.compile(r"\b\d{4}-\d{2}-\d{2}\b")
PATH_RE = re.compile(r"[\w./-]+\.(?:rs|py|ts|toml|ya?ml|md)")


def _norm(s: str) -> str:
    return s.strip().lower().replace(" ", "_").replace("-", "_")


def split_sections(text: str) -> dict[str, str]:
    """Map a normalized section key (a..f) to its body, by header keyword."""
    keymap = [
        ("executive summary", "a"), ("methodology", "b"), ("gap table", "c"),
        ("accuracy", "d"), ("backlog", "e"), ("reconciliation", "f"),
    ]
    out: dict[str, str] = {}
    cur = None
    buf: list[str] = []
    for line in text.splitlines():
        if line.lstrip().startswith("#"):
            hdr = line.lstrip("#").strip().lower()
            matched = next((k for kw, k in keymap if kw in hdr), None)
            if matched:
                if cur:
                    out[cur] = "\n".join(buf)
                cur, buf = matched, []
                continue
        if cur:
            buf.append(line)
    if cur:
        out[cur] = "\n".join(buf)
    return out


def parse_tables(body: str) -> list[tuple[list[str], list[dict[str, str]]]]:
    """Return [(headers, [rowdict,...]), ...] for every GitHub-flavoured table in body."""
    tables = []
    lines = [ln for ln in body.splitlines()]
    i = 0
    while i < len(lines):
        ln = lines[i].strip()
        if ln.startswith("|") and i + 1 < len(lines) and re.match(r"^\|[\s:|-]+\|?$", lines[i + 1].strip()):
            headers = [_norm(c) for c in ln.strip("|").split("|")]
            rows = []
            j = i + 2
            while j < len(lines) and lines[j].strip().startswith("|"):
                cells = [c.strip() for c in lines[j].strip().strip("|").split("|")]
                if len(cells) >= len(headers):
                    rows.append(dict(zip(headers, cells)))
                j += 1
            tables.append((headers, rows))
            i = j
        else:
            i += 1
    return tables


def _find_table(sections: dict[str, str], key: str, required_cols: set[str]):
    for headers, rows in parse_tables(sections.get(key, "")):
        if required_cols.issubset(set(headers)):
            return rows
    return None


def _path_exists_nontest(cell: str) -> tuple[bool, bool]:
    """(path_resolves, is_production_nontest) for the first path-shaped token in cell."""
    m = PATH_RE.search(cell)
    if not m:
        return (False, False)
    p = m.group(0)
    exists = (REPO_ROOT / p).exists()
    nontest = "/tests/" not in f"/{p}" and not p.endswith("_test.py") and not re.search(r"tests?/", p)
    return (exists, exists and nontest)


def validate(path: Path) -> list[str]:
    errors: list[str] = []
    if not path.exists():
        return ["incomplete_coverage:doc-missing"]
    text = path.read_text(encoding="utf-8")
    sections = split_sections(text)

    # §C gap table — coverage, evidence, built-not-wired
    gap_cols = {"dimension", "mem0_capability", "lunaris_reality", "evidence_anchor", "mem0_source", "verdict", "severity"}
    gap_rows = _find_table(sections, "c", gap_cols)
    if gap_rows is None:
        errors.append("incomplete_coverage:no-gap-table")
        gap_rows = []
    seen_dims = set()
    for r in gap_rows:
        dim = _norm(r.get("dimension", "")).replace("_", "-")
        seen_dims.add(dim)
        verdict = r.get("verdict", "").strip()
        src = r.get("mem0_source", "")
        if not (URL_RE.search(src) and DATE_RE.search(src)):
            errors.append(f"unsourced_claim:{dim or '?'}")
        resolves, prod = _path_exists_nontest(r.get("evidence_anchor", ""))
        if verdict == "at-parity" and not prod:
            errors.append(f"unwired_claim:{dim or '?'}")
        elif verdict not in VERDICTS:
            errors.append(f"unwired_claim:bad-verdict:{dim or '?'}")
    missing = REQUIRED_DIMENSIONS - seen_dims
    if missing:
        errors.append("incomplete_coverage:" + ",".join(sorted(missing)))

    # §D accuracy — numbers need methodology
    d = sections.get("d", "")
    if NUMBER_RE.search(d) and not any(m in d.lower() for m in METHODOLOGY_MARKERS):
        errors.append("apples_to_oranges")

    # §E backlog — every item actionable
    bk_cols = {"proposed_task_slug", "dimension", "severity", "impact", "acceptance_evidence", "rough_effort", "depends_on"}
    bk_rows = _find_table(sections, "e", bk_cols)
    if bk_rows is None:
        errors.append("dangling_p0:no-backlog-table")
        bk_rows = []
    for r in bk_rows:
        if not all(r.get(c, "").strip() and r.get(c, "").strip() != "-"
                   for c in ("proposed_task_slug", "impact", "acceptance_evidence", "rough_effort")):
            errors.append(f"dangling_p0:{r.get('proposed_task_slug', '?') or '?'}")
        if r.get("severity", "").strip() not in SEVERITIES:
            errors.append(f"dangling_p0:bad-severity:{r.get('proposed_task_slug', '?')}")

    # §F reconciliation — every prior claim confirmed/corrected
    rc_cols = {"existing_doc", "prior_claim", "status", "note"}
    rc_rows = _find_table(sections, "f", rc_cols)
    if rc_rows is None:
        errors.append("incomplete_coverage:no-reconciliation-table")
    else:
        for r in rc_rows:
            if r.get("status", "").strip().lower() not in {"confirmed", "corrected"}:
                errors.append(f"incomplete_coverage:unreconciled:{r.get('existing_doc', '?')}")

    return errors


def main(argv: list[str]) -> int:
    doc = Path(argv[1]) if len(argv) > 1 else DEFAULT_DOC
    errs = validate(doc)
    if errs:
        print(f"FAIL ({doc}):")
        for e in errs:
            print(f"  - {e}")
        return 1
    print(f"OK ({doc}): all §1 rules satisfied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
