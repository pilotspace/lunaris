#!/usr/bin/env python3
"""Install Lunaris into Codex and Claude Code configs.

Use --runner local for the checked-out workspace. The uvx/npx runner modes write
portable MCP commands for the packaged distribution once the PyPI/npm packages
are published. Codex hook injection currently uses the local checkout because
the hook/contextd binaries and Codex adapter are repo-local surfaces.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "scripts" / "lunaris-codex-hook-adapter.py"
HOOK_BIN = ROOT / "target" / "release" / "lunaris-hook"
CONTEXTD_BIN = ROOT / "target" / "release" / "lunaris-contextd"
MOON_MANIFEST = ROOT / "vendor" / "moon" / "Cargo.toml"
MOON_BIN = ROOT / "vendor" / "moon" / "target" / "release" / "moon"
DEFAULT_GGUF = Path.home() / ".lunaris" / "models" / "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf"
DEFAULT_MOON_URL = "moon://127.0.0.1:6380"

CODEX_HOOK_EVENTS = [
    "session_start",
    "user_prompt_submit",
    "pre_tool_use",
    "post_tool_use",
    "pre_compact",
    "post_compact",
    "subagent_start",
    "subagent_stop",
    "stop",
]

CLAUDE_HOOK_EVENTS = [
    "SessionStart",
    "UserPromptSubmit",
    "UserPromptExpansion",
    "PreToolUse",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
    "Stop",
]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--agent",
        choices=("codex", "claude", "both"),
        default="both",
        help="Which agent config to update.",
    )
    parser.add_argument(
        "--runner",
        choices=("uvx", "npx", "local"),
        default="local",
        help="How to run lunaris-mcp.",
    )
    parser.add_argument(
        "--hooks",
        choices=("on", "off"),
        default="on",
        help="Install hook capture/context injection where supported.",
    )
    parser.add_argument(
        "--build-hooks",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Build local lunaris-hook/lunaris-contextd release binaries when hooks are on.",
    )
    parser.add_argument(
        "--local-mcp",
        default=str(ROOT / "target" / "release" / "lunaris-mcp"),
        help="Path used when --runner local is selected.",
    )
    parser.add_argument(
        "--codex-config",
        default=str(Path.home() / ".codex" / "config.toml"),
        help="Codex config path.",
    )
    parser.add_argument(
        "--claude-settings",
        default=str(Path.home() / ".claude" / "settings.json"),
        help="Claude Code settings path.",
    )
    parser.add_argument(
        "--storage-url",
        default=None,
        help=(
            "Storage URL shared by MCP and hooks. Defaults to "
            f"{DEFAULT_MOON_URL}; use --storage-backend sqlite for per-scope SQLite."
        ),
    )
    parser.add_argument(
        "--storage-backend",
        choices=("moon", "sqlite"),
        default="moon",
        help="Default storage backend when --storage-url is omitted.",
    )
    parser.add_argument(
        "--moon-url",
        default=DEFAULT_MOON_URL,
        help="Moon URL used when --storage-backend moon and --storage-url is omitted.",
    )
    parser.add_argument(
        "--moon-full-features",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Enable Moon-oriented graph ingest/search env knobs when using moon:// storage.",
    )
    parser.add_argument(
        "--build-moon",
        action=argparse.BooleanOptionalAction,
        default=False,
        help=(
            "Build the vendored Moon server in release mode with default features: "
            "mq, graph, and text-index."
        ),
    )
    parser.add_argument(
        "--moon-bin",
        default=str(MOON_BIN),
        help="Expected Moon server binary path after --build-moon.",
    )
    parser.add_argument(
        "--scope",
        default=None,
        help="Optional fixed Lunaris scope name.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print target changes without writing files.",
    )
    args = parser.parse_args()

    if args.build_moon:
        ensure_moon_prereqs(args)
    if args.hooks == "on":
        ensure_hook_prereqs(args)

    mcp = mcp_command(args)

    if args.agent in {"codex", "both"}:
        update_codex(Path(args.codex_config), args, mcp)
    if args.agent in {"claude", "both"}:
        update_claude(Path(args.claude_settings), args, mcp)

    print("Lunaris setup complete.")
    if args.agent in {"codex", "both"}:
        print(f"- Codex config: {args.codex_config}")
    if args.agent in {"claude", "both"}:
        print(f"- Claude settings: {args.claude_settings}")
    if args.hooks == "on":
        print("- Hooks: enabled where supported")
    else:
        print("- Hooks: skipped")
    if args.build_moon:
        print(f"- Moon: built at {args.moon_bin}")
    return 0


def ensure_moon_prereqs(args: argparse.Namespace) -> None:
    if not MOON_MANIFEST.exists():
        raise SystemExit(f"Vendored Moon manifest not found: {MOON_MANIFEST}")
    run(
        ["cargo", "build", "--manifest-path", str(MOON_MANIFEST), "--release"],
        cwd=ROOT,
        dry_run=args.dry_run,
    )
    if not args.dry_run and not Path(args.moon_bin).exists():
        raise SystemExit(f"Moon binary is missing after build: {args.moon_bin}")


def ensure_hook_prereqs(args: argparse.Namespace) -> None:
    if not ADAPTER.exists():
        raise SystemExit(f"Codex hook adapter not found: {ADAPTER}")
    if args.build_hooks and (not HOOK_BIN.exists() or not CONTEXTD_BIN.exists()):
        run(["cargo", "build", "--release", "-p", "lunaris-hook"], cwd=ROOT, dry_run=args.dry_run)
    if not args.dry_run and (not HOOK_BIN.exists() or not CONTEXTD_BIN.exists()):
        raise SystemExit(
            "hook binaries are missing. Run: cargo build --release -p lunaris-hook"
        )


def mcp_command(args: argparse.Namespace) -> dict[str, Any]:
    if args.runner == "uvx":
        require_command("uvx")
        return {"command": "uvx", "args": ["lunaris-mcp"]}
    if args.runner == "npx":
        require_command("npx")
        return {"command": "npx", "args": ["-y", "@pilotspace/lunaris-mcp"]}
    path = Path(args.local_mcp)
    if args.build_hooks and not path.exists():
        run(["cargo", "build", "--release", "-p", "lunaris-mcp"], cwd=ROOT, dry_run=args.dry_run)
    return {"command": str(path), "args": []}


def require_command(name: str) -> None:
    if shutil.which(name) is None:
        raise SystemExit(f"{name!r} not found on PATH")


def update_codex(path: Path, args: argparse.Namespace, mcp: dict[str, Any]) -> None:
    original = path.read_text() if path.exists() else ""
    text = remove_toml_table(original, "mcp_servers.lunaris")
    text = remove_toml_table(text, "mcp_servers.lunaris.env")
    if args.hooks == "on":
        text = ensure_hooks_table(remove_codex_hook_assignments(text))

    blocks: list[str] = []
    blocks.append(render_codex_mcp(args, mcp))
    if args.hooks == "on":
        text = inject_codex_hooks(text, args)
    final = text.rstrip() + "\n\n" + "\n\n".join(blocks).rstrip() + "\n"
    write_file(path, final, args.dry_run)


def render_codex_mcp(args: argparse.Namespace, mcp: dict[str, Any]) -> str:
    lines = [
        "[mcp_servers.lunaris]",
        f"command = {json.dumps(mcp['command'])}",
        f"args = {json.dumps(mcp['args'])}",
    ]
    env = mcp_env(args, hook_env=False)
    if env:
        lines.append("")
        lines.append("[mcp_servers.lunaris.env]")
        for key, value in env.items():
            lines.append(f"{key} = {json.dumps(value)}")
    return "\n".join(lines)


def mcp_env(args: argparse.Namespace, hook_env: bool) -> dict[str, str]:
    env: dict[str, str] = {}
    if args.scope:
        env["LUNARIS_MCP_SCOPE" if not hook_env else "LUNARIS_HOOK_SCOPE"] = args.scope
    storage_url = effective_storage_url(args)
    if storage_url:
        env["LUNARIS_MCP_STORAGE" if not hook_env else "LUNARIS_STORE_URL"] = storage_url
    if moon_features_enabled(args, storage_url):
        env["LUNARIS_GRAPH_ENABLED"] = "1"
    if DEFAULT_GGUF.exists():
        env["LUNARIS_EMBEDDER_GGUF"] = str(DEFAULT_GGUF)
    return env


def effective_storage_url(args: argparse.Namespace) -> str | None:
    if args.storage_url:
        return args.storage_url
    if args.storage_backend == "moon":
        return args.moon_url
    return None


def moon_features_enabled(args: argparse.Namespace, storage_url: str | None) -> bool:
    return bool(
        args.moon_full_features
        and storage_url
        and storage_url.strip().lower().startswith("moon://")
    )


def remove_toml_table(text: str, table: str) -> str:
    lines = text.splitlines()
    out: list[str] = []
    skip = False
    target = f"[{table}]"
    for line in lines:
        stripped = line.strip()
        if stripped == target:
            skip = True
            continue
        if skip and stripped.startswith("[") and stripped.endswith("]"):
            skip = False
        if not skip:
            out.append(line)
    return "\n".join(out).rstrip() + ("\n" if out else "")


def remove_codex_hook_assignments(text: str) -> str:
    lines = text.splitlines()
    out: list[str] = []
    i = 0
    while i < len(lines):
        stripped = lines[i].strip()
        matched = next((event for event in CODEX_HOOK_EVENTS if stripped.startswith(f"{event} = [")), None)
        if matched:
            depth = 0
            while i < len(lines):
                depth += lines[i].count("[")
                depth -= lines[i].count("]")
                i += 1
                if depth <= 0:
                    break
            continue
        out.append(lines[i])
        i += 1
    return "\n".join(out).rstrip() + ("\n" if out else "")


def ensure_hooks_table(text: str) -> str:
    if any(line.strip() == "[hooks]" for line in text.splitlines()):
        return text
    return text.rstrip() + "\n\n[hooks]\n"


def inject_codex_hooks(text: str, args: argparse.Namespace) -> str:
    lines = text.splitlines()
    out: list[str] = []
    inserted = False
    for idx, line in enumerate(lines):
        out.append(line)
        if line.strip() == "[hooks]":
            out.extend(render_codex_hook_arrays(args))
            inserted = True
    if not inserted:
        out.append("[hooks]")
        out.extend(render_codex_hook_arrays(args))
    return "\n".join(out).rstrip() + "\n"


def hook_env(args: argparse.Namespace) -> dict[str, str]:
    env: dict[str, str] = {}
    if args.scope:
        env["LUNARIS_HOOK_SCOPE"] = args.scope
    storage_url = effective_storage_url(args)
    if storage_url:
        env["LUNARIS_STORE_URL"] = storage_url
    if moon_features_enabled(args, storage_url):
        env["LUNARIS_GRAPH_ENABLED"] = "1"
        env["LUNARIS_EMBED_PROMOTION_ENABLED"] = "1"
        env["LUNARIS_EMBED_PROMOTION_WORKER"] = "1"
        env["LUNARIS_EMBED_BATCH_SIZE"] = "16"
        env["LUNARIS_EMBED_BATCH_WAIT_MS"] = "25"
        env["LUNARIS_CONTEXT_CAPTURE_FAST"] = "1"
    if DEFAULT_GGUF.exists():
        env["LUNARIS_EMBEDDER_GGUF"] = str(DEFAULT_GGUF)
    return env


def env_prefix(env: dict[str, str]) -> str:
    if not env:
        return ""
    parts = ["env"]
    for key, value in env.items():
        parts.append(f"{key}={shlex.quote(value)}")
    return " ".join(parts) + " "


def render_codex_hook_arrays(args: argparse.Namespace) -> list[str]:
    adapter = shlex.quote(str(ADAPTER))
    prefix = env_prefix(hook_env(args))
    capture_cmd = f"{prefix}{adapter} --mode capture"
    inject_cmd = f"{prefix}{adapter} --mode inject"
    post_tool_cmd = f"{prefix}{adapter} --mode post-tool"
    feedback_cmd = f"{prefix}{adapter} --mode feedback"
    one_capture = f'{{ type = "command", command = {json.dumps(capture_cmd)}, timeout = 2, async = true, statusMessage = "Lunaris memory capture" }}'
    arrays = {
        "session_start": [one_capture],
        "user_prompt_submit": [
            one_capture,
            f'{{ type = "command", command = {json.dumps(inject_cmd)}, timeout = 2, async = false, statusMessage = "Lunaris memory recall" }}',
        ],
        "pre_tool_use": [one_capture],
        "post_tool_use": [
            one_capture,
            f'{{ type = "command", command = {json.dumps(post_tool_cmd)}, timeout = 2, async = false, statusMessage = "Lunaris post-tool memory recall" }}',
        ],
        "pre_compact": [one_capture],
        "post_compact": [one_capture],
        "subagent_start": [one_capture],
        "subagent_stop": [one_capture],
        "stop": [
            one_capture,
            f'{{ type = "command", command = {json.dumps(feedback_cmd)}, timeout = 2, async = true, statusMessage = "Lunaris memory feedback" }}',
        ],
    }
    rendered: list[str] = []
    for event in CODEX_HOOK_EVENTS:
        rendered.append(f"{event} = [")
        rendered.append(f'  {{ matcher = "", hooks = [{", ".join(arrays[event])}] }},')
        rendered.append("]")
    return rendered


def update_claude(path: Path, args: argparse.Namespace, mcp: dict[str, Any]) -> None:
    data: dict[str, Any]
    if path.exists():
        data = json.loads(path.read_text())
    else:
        data = {}
    data.setdefault("mcpServers", {})
    data["mcpServers"]["lunaris"] = {
        "command": mcp["command"],
        "args": mcp["args"],
    }
    env = mcp_env(args, hook_env=False)
    if env:
        data["mcpServers"]["lunaris"]["env"] = env
    if args.hooks == "on":
        data.setdefault("hooks", {})
        for event, hooks in render_claude_hook_entries(args).items():
            data["hooks"][event] = [{"matcher": "", "hooks": hooks}]
    final = json.dumps(data, indent=2, sort_keys=True) + "\n"
    write_file(path, final, args.dry_run)


def render_claude_hook_entries(args: argparse.Namespace) -> dict[str, list[dict[str, str]]]:
    adapter = shlex.quote(str(ADAPTER))
    prefix = env_prefix(hook_env(args))
    capture_cmd = f"{prefix}{adapter} --target claude --mode capture"
    inject_cmd = f"{prefix}{adapter} --target claude --mode inject"
    post_tool_cmd = f"{prefix}{adapter} --target claude --mode post-tool"
    feedback_cmd = f"{prefix}{adapter} --target claude --mode feedback"

    def command(cmd: str) -> dict[str, str]:
        return {"type": "command", "command": cmd}

    capture = command(capture_cmd)
    entries: dict[str, list[dict[str, str]]] = {
        "SessionStart": [capture],
        "UserPromptSubmit": [capture, command(inject_cmd)],
        "UserPromptExpansion": [capture, command(inject_cmd)],
        "PreToolUse": [capture],
        "PostToolUse": [capture, command(post_tool_cmd)],
        "PreCompact": [capture],
        "PostCompact": [capture],
        "SubagentStart": [capture],
        "SubagentStop": [capture, command(post_tool_cmd)],
        "Stop": [capture, command(feedback_cmd)],
    }
    return {event: entries[event] for event in CLAUDE_HOOK_EVENTS}


def write_file(path: Path, content: str, dry_run: bool) -> None:
    if dry_run:
        print(f"--- {path} (dry-run) ---")
        print(content.rstrip())
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        backup = path.with_suffix(path.suffix + ".bak")
        backup.write_text(path.read_text())
    path.write_text(content)


def run(cmd: list[str], cwd: Path, dry_run: bool) -> None:
    if dry_run:
        print("+", " ".join(cmd))
        return
    subprocess.run(cmd, cwd=str(cwd), check=True)


if __name__ == "__main__":
    raise SystemExit(main())
