#!/usr/bin/env python3
"""Count every parked test in `crates/`, whatever mechanism parks it.

The shell ratchet this replaces greps `#\\[ignore` over `crates/*/tests/*.rs`.
That is blind two ways, and both blind spots were occupied:

  * `#[cfg_attr(not(feature = "x"), ignore = "...")]` never contains the
    literal `#[ignore`, and when it spans lines a line-oriented grep cannot
    see it at all. `binary_size_gate.rs` parks that way — and its feature is
    enabled by no workflow, so it was the least-covered test in the repo
    while the ratchet reported a matching count.
  * `*/tests/*` skips `src/`, where `#[cfg(test)]` unit tests live.
"""
import re, sys, pathlib

def uncommented(src):
    """Blank out // and /* */ comments, preserving line count."""
    out, in_block = [], False
    for line in src.splitlines():
        s = line
        if in_block:
            if '*/' in s:
                s = s.split('*/', 1)[1]; in_block = False
            else:
                out.append(''); continue
        if '/*' in s and '*/' not in s:
            s = s.split('/*', 1)[0]; in_block = True
        if s.lstrip().startswith('//'):
            s = ''
        out.append(s)
    return out

ATTR = re.compile(r'#\[\s*(?:cfg_attr\s*\(.*?,\s*)?ignore\b', re.S)

def sites(root='crates'):
    found = []
    for f in sorted(pathlib.Path(root).rglob('*.rs')):
        # Never walk build output: a stray `target/` under crates/ would add
        # tens of thousands of generated files and could carry `#[ignore]`
        # attributes that are not source.
        if 'target' in f.parts:
            continue
        lines = uncommented(f.read_text(errors='replace'))
        i = 0
        while i < len(lines):
            line = lines[i]
            if '#[' in line:
                chunk, j = line, i
                # accumulate until the attribute's brackets balance
                while chunk.count('[') > chunk.count(']') and j + 1 < len(lines):
                    j += 1; chunk += ' ' + lines[j].strip()
                if ATTR.search(chunk):
                    name = None
                    for k in range(j, min(j + 14, len(lines))):
                        mm = re.search(r'\bfn\s+([A-Za-z0-9_]+)', lines[k])
                        if mm:
                            name = mm.group(1); break
                    found.append((str(f), i + 1, ' '.join(chunk.split()), name))
                    i = j
            i += 1
    return found

USAGE = """usage: scripts/ratchet-parked-tests.py [manifest]

Run it locally exactly as CI does:

    python3 scripts/ratchet-parked-tests.py

Default manifest: ci/parked-tests.txt
"""

MANIFEST = 'ci/parked-tests.txt'


def read_manifest(path):
    entries = {}
    for raw in pathlib.Path(path).read_text().splitlines():
        line = raw.split('#', 1)[0].strip()
        if not line:
            continue
        parts = line.split(None, 2)
        if len(parts) < 3 or parts[1] not in ('RUN_BY', 'NEVER_RUN'):
            sys.exit(f'::error::{path}: malformed line (want "<file>::<fn> RUN_BY|NEVER_RUN <detail>"): {raw}')
        entries[parts[0]] = (parts[1], parts[2])
    return entries


if __name__ == '__main__':
    if len(sys.argv) > 2:
        sys.stderr.write(USAGE); sys.exit(2)
    manifest_path = sys.argv[1] if len(sys.argv) == 2 else MANIFEST
    manifest = read_manifest(manifest_path)

    found = sites()
    rc = 0
    keys = []
    print(f'parked tests in crates/: {len(found)}')
    for f, ln, txt, name in found:
        key = f'{f}::{name}'
        keys.append(key)
        status = manifest.get(key, ('UNLISTED', ''))[0]
        print(f'  [{status:9}] {key}  (L{ln})')
        if not re.search(r'ignore\s*=\s*"', txt):
            print(f'::error::{key} is parked with no reason. '
                  'Write #[ignore = "what it needs / what un-ignores it"].')
            rc = 1
        if key not in manifest:
            print(f'::error::{key} is parked but absent from {manifest_path}. '
                  'Add a line saying which job un-parks it (RUN_BY) or that none does '
                  '(NEVER_RUN) — a parked test nobody runs is not coverage.')
            rc = 1

    for key in manifest:
        if key not in keys:
            print(f'::error::{manifest_path} lists {key}, which is no longer parked. '
                  'Delete the line.')
            rc = 1

    run_by = sum(1 for k in keys if manifest.get(k, ('', ''))[0] == 'RUN_BY')
    never = sum(1 for k in keys if manifest.get(k, ('', ''))[0] == 'NEVER_RUN')
    print(f'summary: {len(keys)} parked — {run_by} un-parked by a job, {never} run nowhere')
    sys.exit(rc)
