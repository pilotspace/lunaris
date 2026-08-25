#!/usr/bin/env bash
# Restore `resolved` + `integrity` on the @pilotspace/lunaris-* platform
# entries in crates/lunaris-ts/package-lock.json, AFTER the version is
# published to npm.
#
# WHY A SCRIPT AND NOT ONE npm COMMAND
# ------------------------------------
# scripts/bump-version.sh deliberately DELETES `resolved` and `integrity`
# from the five platform entries: those fields are a URL and a sha512 of the
# published tarball, and at bump time the tarball does not exist yet. Leaving
# stale hashes makes `npm ci` fail EUSAGE.
#
# bump-version.sh then told the operator to run, after publishing:
#
#     npm install --package-lock-only
#
# That instruction is WRONG and it fails SILENTLY. Measured against the real
# v0.7.1 publish: npm printed "found 0 vulnerabilities", exited 0, and wrote
# NOTHING — the file was byte-identical afterwards. The lockfile already
# names version 0.7.1 for every platform package, so npm considers the tree
# satisfied and does no resolution work at all. `--prefer-online` does not
# change this; there is no request to make, because npm sees nothing to do.
#
# The fix is to make the tree genuinely unsatisfied first: DELETE the five
# platform entries, then let npm re-resolve them from the registry. npm then
# writes back both fields.
#
# This is the shape where a command that reports success and changes nothing
# is indistinguishable from one that worked — so this script does not trust
# npm's exit code. It asserts, on the resulting file, that all five entries
# carry both fields, and exits non-zero if any does not.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TS_DIR="$ROOT/crates/lunaris-ts"
LOCK="$TS_DIR/package-lock.json"

[[ -f "$LOCK" ]] || { echo "!! no lockfile at $LOCK" >&2; exit 1; }

VER="$(node -p "require('$TS_DIR/package.json').version")"
echo "==> restoring integrity for @pilotspace/lunaris-* at $VER"

# 1. Drop the platform entries so npm has real work to do.
python3 - "$LOCK" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
gone = [k for k in d["packages"] if k.startswith("node_modules/@pilotspace/lunaris-")]
if not gone:
    print("   (no platform entries present; npm will add them)")
for k in gone:
    del d["packages"][k]
json.dump(d, open(p, "w"), indent=2)
open(p, "a").write("\n")
print(f"   cleared {len(gone)} platform entries")
PY

# 2. Re-resolve from the registry.
( cd "$TS_DIR" && npm install --package-lock-only --prefer-online >/dev/null )

# 3. Assert the result. npm's exit code is not evidence here.
python3 - "$LOCK" "$VER" <<'PY'
import json, sys
p, ver = sys.argv[1], sys.argv[2]
d = json.load(open(p))
entries = {k: v for k, v in d["packages"].items()
           if k.startswith("node_modules/@pilotspace/lunaris-")}
if len(entries) != 5:
    sys.exit(f"!! expected 5 platform entries, got {len(entries)}: {sorted(entries)}")
bad = []
for k, v in sorted(entries.items()):
    name = k.split("/")[-1]
    if v.get("version") != ver:
        bad.append(f"{name}: version {v.get('version')} != {ver}")
    if not v.get("resolved"):
        bad.append(f"{name}: no `resolved`")
    if not str(v.get("integrity", "")).startswith("sha512-"):
        bad.append(f"{name}: no sha512 `integrity`")
if bad:
    sys.exit("!! lockfile repair did not take:\n   " + "\n   ".join(bad) +
             "\n   Are all five packages published at this version?")
for k, v in sorted(entries.items()):
    print(f"   ok {k.split('/')[-1]:26} {v['integrity'][:28]}…")
PY

echo "==> done. Verify with: (cd crates/lunaris-ts && npm ci)"
