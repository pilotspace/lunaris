#!/usr/bin/env python3
"""Derive crates.io publish order for the 15 lunaris crates, including
dev-dependencies (versioned dev-deps are kept in the published manifest and
crates.io validates they exist)."""
import json
import subprocess
from graphlib import TopologicalSorter

PUBLISH = {
    "lunaris-core", "lunaris-llm", "lunaris-extract", "lunaris-consolidate",
    "lunaris-embed-native", "lunaris-embed-remote", "lunaris-ingest",
    "lunaris-rerank", "lunaris-rerank-native", "lunaris-storage-moon",
    "lunaris-retrieve", "lunaris-storage-embedded", "lunaris-storage-postgres",
    "lunaris-verify", "lunaris-memory",
}

meta = json.loads(subprocess.run(
    ["cargo", "metadata", "--no-deps", "--format-version", "1"],
    capture_output=True, text=True, check=True).stdout)

graph = {}          # crate -> set of intra-publish deps (normal+build+dev)
dev_edges = []      # (crate, dev-dep) pairs for reporting
for p in meta["packages"]:
    if p["name"] not in PUBLISH:
        continue
    deps = set()
    for d in p["dependencies"]:
        if d["name"] in PUBLISH:
            deps.add(d["name"])
            if d["kind"] == "dev":
                dev_edges.append((p["name"], d["name"], d.get("req", "")))
    graph[p["name"]] = deps

print("dev-dep edges (crate -> dev-dep, req):  [req=* is stripped at publish]")
for a, b, r in sorted(dev_edges):
    print(f"  {a} -> {b}  req={r}")

try:
    order = list(TopologicalSorter(graph).static_order())
    print("\npublish order (deps incl. dev):")
    for i, c in enumerate(order, 1):
        print(f"  {i:2d}. {c}")
except Exception as e:  # cycle
    print(f"\nCYCLE: {e}")

# Deps from the publish set onto workspace crates NOT being published would
# hard-fail at the registry (crates.io validates every versioned dep exists).
members = {p["name"] for p in meta["packages"]}
print("\ncross-set check (publish-set -> unpublished workspace crate):")
bad = False
for p in meta["packages"]:
    if p["name"] not in PUBLISH:
        continue
    for d in p["dependencies"]:
        if d["name"] in members and d["name"] not in PUBLISH \
                and d["name"].startswith("lunaris"):
            print(f'  {p["name"]} -[{d["kind"] or "normal"}]-> '
                  f'{d["name"]} req={d.get("req", "")}')
            bad = True
print("  CLEAN — none" if not bad else "  PROBLEM EDGES ABOVE")
