"""Which feature-gated tests does any workflow actually run?

The `#[ignore]` ratchet is blind to a THIRD parking mechanism, and the repo was
using it: a test gated on a Cargo feature that no `cargo test` in
`.github/workflows/` ever enables. `crash_recovery.rs` sat behind `chaos-it`
that way — a whole file plus a placeholder test standing in for it, so
`cargo test --workspace` printed a green `1 passed` for a suite that was not
compiled. Neither the manifest nor a count-based ratchet could see it.

Coverage here is computed, not asserted: parse every `cargo test` in the
workflows for `-p` / `--features` / `--test` / `--workspace` / `--exclude` /
`--no-default-features`, resolve each crate's feature closure (defaults
included, transitively), then EVALUATE the test's real `cfg` expression against
it. Evaluating rather than string-matching is what makes `#[cfg(not(feature =
"embedded-moon"))]` come out as "runs by default" instead of "runs nowhere" —
a substring check reports the opposite, and reported it while I was writing
this.

Non-feature predicates resolve from KNOWN. An unrecognised one is REPORTED,
never silently assumed away: a gate that quietly treats what it cannot parse as
covered is the exact defect this file exists to find.
"""

import pathlib
import re
import shlex
import tomllib

CRATES = pathlib.Path("crates")
WORKFLOWS = pathlib.Path(".github/workflows")

# CI runners are Linux/macOS, and `cargo test` without `--release` is debug.
KNOWN = {
    "unix": True,
    "windows": False,
    "debug_assertions": True,
    "test": True,
    'target_family="unix"': True,
    'target_os="linux"': True,
    'target_os="macos"': True,
    'target_os="windows"': False,
}

TEST_ATTR = re.compile(r"#\[\s*(?:tokio::)?test\b|#\[\s*test_log|#\[\s*rstest")
ITEM_NAME = re.compile(r"\b(?:mod|fn)\s+([A-Za-z0-9_]+)")
CFG_OPEN = re.compile(r"#(!?)\[\s*cfg\s*\(")
CFG_FULL = re.compile(r"#!?\[\s*cfg\s*\((.*)\)\s*\]", re.S)
FEATURE_PRED = re.compile(r'^feature\s*=\s*"([^"]+)"$')


def parse_cfg(text):
    """A cfg predicate string -> a tree of ('all'|'any'|'not', [...]) / ('atom', s)."""
    text = text.strip()
    m = re.match(r"^(all|any|not)\s*\((.*)\)$", text, re.S)
    if not m:
        return ("atom", text)
    kind, inner = m.group(1), m.group(2)
    parts, depth, cur = [], 0, ""
    for ch in inner:
        if ch == "," and depth == 0:
            parts.append(cur)
            cur = ""
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        cur += ch
    if cur.strip():
        parts.append(cur)
    return (kind, [parse_cfg(p) for p in parts])


def eval_cfg(tree, enabled, unknown):
    kind, val = tree
    if kind == "all":
        return all(eval_cfg(t, enabled, unknown) for t in val)
    if kind == "any":
        return any(eval_cfg(t, enabled, unknown) for t in val)
    if kind == "not":
        return not eval_cfg(val[0], enabled, unknown)
    m = FEATURE_PRED.match(val.strip())
    if m:
        return m.group(1) in enabled
    key = re.sub(r"\s*=\s*", "=", val.strip())
    if key in KNOWN:
        return KNOWN[key]
    unknown.add(key)
    return True


def mentions_a_feature(tree):
    kind, val = tree
    if kind in ("all", "any", "not"):
        return any(mentions_a_feature(t) for t in val)
    return bool(FEATURE_PRED.match(val.strip()))


def crates():
    """dir -> (package name, feature table, default-feature closure)."""
    out = {}
    for tf in sorted(CRATES.glob("*/Cargo.toml")):
        doc = tomllib.loads(tf.read_text())
        name = doc.get("package", {}).get("name")
        if not name:
            continue
        table = {
            k: [x for x in v if not x.startswith("dep:")]
            for k, v in doc.get("features", {}).items()
        }
        closure, stack = set(), list(table.get("default", []))
        while stack:
            f = stack.pop()
            if f in closure:
                continue
            closure.add(f)
            stack += table.get(f, [])
        out[tf.parent.name] = (name, table, closure)
    return out


def invocations():
    """Every `cargo test` a workflow runs, as a dict of what it selects."""
    out = []
    for wf in sorted(WORKFLOWS.glob("*.yml")):
        joined = re.sub(r"\\\s*\n\s*", " ", wf.read_text())
        for line in joined.splitlines():
            s = line.strip()
            # `- name: cargo test ...` is a label, not a command.
            if re.match(r"^-?\s*name\s*:", s):
                continue
            s = re.sub(r"^-\s*", "", s)
            s = re.sub(r"^run\s*:\s*", "", s)
            if s.startswith("#") or not re.search(r"\bcargo\s+test\b", s):
                continue
            try:
                toks = shlex.split(s)
            except ValueError:
                toks = s.split()
            if "cargo" not in toks:
                continue
            toks = toks[toks.index("cargo") :]
            pkgs, feats, tests, excl = set(), set(), set(), set()
            workspace = no_default = False
            i = 0
            while i < len(toks):
                t = toks[i]
                if t in ("-p", "--package") and i + 1 < len(toks):
                    pkgs.add(toks[i + 1])
                    i += 1
                elif t == "--exclude" and i + 1 < len(toks):
                    excl.add(toks[i + 1])
                    i += 1
                elif t.startswith("--features="):
                    feats |= set(re.split(r"[,\s]+", t.split("=", 1)[1]))
                elif t == "--features" and i + 1 < len(toks):
                    feats |= set(re.split(r"[,\s]+", toks[i + 1]))
                    i += 1
                elif t == "--all-features":
                    feats.add("*")
                elif t == "--no-default-features":
                    no_default = True
                elif t == "--test" and i + 1 < len(toks):
                    tests.add(toks[i + 1])
                    i += 1
                elif t == "--workspace":
                    workspace = True
                i += 1
            if not pkgs and not workspace:
                continue
            out.append(
                {
                    "wf": wf.name,
                    "pkgs": pkgs,
                    "feats": feats,
                    "tests": tests or None,
                    "workspace": workspace,
                    "exclude": excl,
                    "no_default": no_default,
                }
            )
    return out


def _enabled(crate, inv):
    _, table, default_closure = crate
    seed = set(inv["feats"]) | (set() if inv["no_default"] else default_closure)
    stack, closure = list(seed), set()
    while stack:
        f = stack.pop()
        if f in closure:
            continue
        closure.add(f)
        stack += table.get(f, [])
    return closure


def scan():
    """-> (parked, covered_count, unknown_predicates)

    `parked` is [(key, kind, attr_text, line)] for every feature-gated test
    site whose cfg is false under every workflow invocation that could reach it.
    """
    crate_map, invs = crates(), invocations()
    # A vacuity floor. Every path here is relative to the CWD, so running this
    # from anywhere but the repo root would find no crates and no workflows and
    # report "0 parked" — a clean pass that checked nothing, which is the exact
    # failure shape this module was written to catch.
    if not crate_map:
        raise SystemExit(
            "::error::_featgate found no crates under ./crates — run from the "
            "repo root. Reporting zero gated tests from an empty scan is a "
            "false pass."
        )
    if not invs:
        raise SystemExit(
            "::error::_featgate parsed no `cargo test` invocation out of "
            f"{WORKFLOWS}/ — every gated test would look parked. Fix the parse "
            "before trusting the result."
        )
    unknown, parked, covered = set(), [], 0

    def cover(cdir, stem, tree):
        name = crate_map[cdir][0]
        for inv in invs:
            reachable = name in inv["pkgs"] or (
                inv["workspace"] and name not in inv["exclude"]
            )
            if not reachable:
                continue
            if inv["tests"] is not None and stem not in inv["tests"]:
                continue
            enabled = _enabled(crate_map[cdir], inv)
            if "*" in enabled or eval_cfg(tree, enabled, unknown):
                return True
        return False

    for path in sorted(CRATES.glob("*/tests/*.rs")):
        cdir, stem = path.parts[1], path.stem
        if cdir not in crate_map:
            continue
        lines = _uncommented(path.read_text(errors="replace"))
        n = len(lines)
        for i, line in enumerate(lines):
            m = CFG_OPEN.search(line)
            if not m:
                continue
            inner = m.group(1) == "!"
            chunk, j = line[m.start() :], i
            while chunk.count("(") > chunk.count(")") and j + 1 < n:
                j += 1
                chunk += " " + lines[j].strip()
            full = CFG_FULL.match(chunk.strip())
            if not full:
                continue
            tree = parse_cfg(full.group(1))
            if not mentions_a_feature(tree):
                continue
            if inner:
                name = "*"
                kind = "file"
            else:
                # The gated item runs from here to the end of its brace block;
                # a `use`/`const` ends at the first `;` and gates no test.
                k, depth, started, body = j, 0, False, []
                while k < n:
                    body.append(lines[k])
                    for ch in lines[k]:
                        if ch == "{":
                            depth += 1
                            started = True
                        elif ch == "}":
                            depth -= 1
                    if started and depth <= 0:
                        break
                    if not started and k > j and lines[k].rstrip().endswith(";"):
                        break
                    k += 1
                blob = "\n".join(body)
                if not TEST_ATTR.search(blob):
                    continue
                nm = ITEM_NAME.search(blob)
                name = nm.group(1) if nm else "?"
                kind = "item"
            if cover(cdir, stem, tree):
                covered += 1
            else:
                parked.append(
                    (f"{path}::{name}", kind, " ".join(chunk.split()), i + 1)
                )
    return parked, covered, sorted(unknown)


def _uncommented(src):
    """Blank out // and /* */ comments, preserving line count.

    Without this, a doc comment that QUOTES a `#[cfg(feature = "…")]` — and one
    in `sdk_feature_forwarding.rs` does, to explain the bug it guards — parses
    as a real gate on the test below it.
    """
    out, in_block = [], False
    for line in src.splitlines():
        s = line
        if in_block:
            if "*/" in s:
                s = s.split("*/", 1)[1]
                in_block = False
            else:
                out.append("")
                continue
        if "/*" in s and "*/" not in s:
            s = s.split("/*", 1)[0]
            in_block = True
        if s.lstrip().startswith("//"):
            s = ""
        out.append(s)
    return out
