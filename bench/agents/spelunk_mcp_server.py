#!/usr/bin/env python3
"""Bench-local stdio MCP server re-exposing agent.py's spelunk tools.

Lets an external coding-agent harness (opencode, claude-code) run a spelunk
condition, so `condition` and `harness` stay independent dimensions
(bench/AGENTS.md principle #1) instead of `spelunk_*` being reachable only
through `--harness none`.

Every tool function and JSON schema here is *imported* from agent.py, never
reimplemented: the `spelunk` condition must mean the same capability
whichever harness carries it, and importing makes that hold by construction
rather than by review. Do not inline a tool body or copy a schema into this
file — a second implementation is free to drift, and drift is exactly what
would make a cross-harness comparison dishonest.

spelunk itself ships no MCP server (there is no `spelunk mcp` subcommand);
this wrapper is bench scaffolding, not a product surface.

Usage (spawned by an MCP client, not run by hand):
    python spelunk_mcp_server.py \\
        --repo-path /path/to/repo \\
        --condition spelunk_search|spelunk_full \\
        [--telemetry-log /path/to/calls.jsonl]
"""

import argparse
import asyncio
import json
import os
import sys
import time
from pathlib import Path

# Every spelunk invocation must be keychain-free regardless of how the MCP
# client spawned us; a macOS Keychain prompt has no TTY to answer it.
os.environ.setdefault("SPELUNK_SECRET_STORE", "file")

# bench/agents/*.py are plain scripts, not an installed package, and an MCP
# client spawns this file by absolute path from an arbitrary cwd.
sys.path.insert(0, str(Path(__file__).resolve().parent))

from agent import BASE_TOOLS, build_dispatch_table, build_tools  # noqa: E402

SERVER_NAME = "spelunk"

# Conditions that expose spelunk tools. "baseline" is deliberately absent:
# on baseline this server must never be spawned at all.
SPELUNK_CONDITIONS = ("spelunk_search", "spelunk_full")


def spelunk_tools_for_condition(condition: str) -> list[dict]:
    """The spelunk tools agent.py would expose for this condition.

    Derived by subtracting agent.py's own BASE_TOOLS from build_tools() —
    never a hand-listed set, so this cannot drift from agent.py. The base
    read_file/run_bash/write_file tools are excluded because each harness
    already ships its own natively; re-exposing them over MCP would hand the
    model two of each.
    """
    if condition not in SPELUNK_CONDITIONS:
        raise ValueError(
            f"condition must be one of {list(SPELUNK_CONDITIONS)} (got: {condition})"
        )
    base_names = {t["function"]["name"] for t in BASE_TOOLS}
    return [
        t for t in build_tools(condition) if t["function"]["name"] not in base_names
    ]


def tool_names_for_condition(condition: str) -> list[str]:
    """Bare tool names, as agent.py's model sees them."""
    return [t["function"]["name"] for t in spelunk_tools_for_condition(condition)]


def mcp_tool_names_for_condition(condition: str) -> list[str]:
    """Namespaced names, as an MCP client's model sees them.

    MCP clients namespace tools `mcp__<server>__<tool>`; the capability is
    agent.py's, the spelling is not. This is the one accepted, documented
    difference from harness=none (see README "Conditions").
    """
    return [f"mcp__{SERVER_NAME}__{n}" for n in tool_names_for_condition(condition)]


def mcp_server_command(
    condition: str,
    repo_path: Path,
    telemetry_log: Path | None = None,
    python_executable: str | None = None,
) -> list[str]:
    """argv an MCP client should spawn to get this server.

    Defaults to sys.executable, not a bare "python3": the harness runs under
    `uv run --with-requirements bench/requirements.txt`, and only that
    interpreter has the `mcp` SDK. A bare "python3" would resolve to the
    host's system interpreter and fail to import.
    """
    cmd = [
        python_executable or sys.executable,
        str(Path(__file__).resolve()),
        "--repo-path",
        str(Path(repo_path).resolve()),
        "--condition",
        condition,
    ]
    if telemetry_log:
        cmd += ["--telemetry-log", str(telemetry_log)]
    return cmd


# ---------------------------------------------------------------------------
# Telemetry
#
# Separates three outcomes a spelunk-labelled cell can't otherwise be told
# apart by: the server never spawned (wiring is broken, the cell is baseline
# with extra latency), it spawned but the model never reached for a tool
# (a real result), or it was used. Only the last two are publishable as
# spelunk cells.
# ---------------------------------------------------------------------------


def _append(telemetry_log: Path | None, record: dict) -> None:
    if not telemetry_log:
        return
    try:
        with open(telemetry_log, "a") as f:
            f.write(json.dumps({**record, "ts": time.time()}) + "\n")
            f.flush()
    except Exception:
        # Telemetry must never take the run down with it.
        pass


def log_server_start(telemetry_log: Path | None, condition: str) -> None:
    _append(telemetry_log, {"event": "server_start", "condition": condition})


def log_tool_call(telemetry_log: Path | None, tool_name: str) -> None:
    _append(telemetry_log, {"event": "tool_call", "tool": tool_name})


def read_telemetry(telemetry_log: Path | None) -> dict:
    """Summarise a telemetry log for the harness's result JSON.

    A missing log is a real, reportable outcome (server never spawned), not
    an error — the client spawns the server itself, so its absence is
    evidence about the wiring rather than a failure to read.
    """
    by_tool: dict[str, int] = {}
    total = 0
    spawned = False
    if telemetry_log and Path(telemetry_log).exists():
        for line in Path(telemetry_log).read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            event = rec.get("event")
            if event == "server_start":
                spawned = True
            elif event == "tool_call" and rec.get("tool"):
                by_tool[rec["tool"]] = by_tool.get(rec["tool"], 0) + 1
                total += 1
    return {
        "spelunk_mcp_server_spawned": spawned,
        "spelunk_tool_calls": total,
        "spelunk_tool_calls_by_tool": by_tool or None,
    }


# ---------------------------------------------------------------------------
# Server
# ---------------------------------------------------------------------------


async def serve(condition: str, repo_path: Path, telemetry_log: Path | None) -> None:
    import mcp.types as types
    from mcp.server import Server
    from mcp.server.stdio import stdio_server

    tools = spelunk_tools_for_condition(condition)
    exposed = {t["function"]["name"] for t in tools}
    dispatch = build_dispatch_table(repo_path)
    server: Server = Server(SERVER_NAME)

    # Recorded before the handshake, so it survives a client that spawns us
    # and disconnects: the question this answers is whether the harness
    # spawned the server at all, not whether a session completed.
    log_server_start(telemetry_log, condition)

    @server.list_tools()
    async def list_tools() -> list[types.Tool]:
        return [
            types.Tool(
                name=t["function"]["name"],
                description=t["function"]["description"],
                inputSchema=t["function"]["parameters"],
            )
            for t in tools
        ]

    @server.call_tool()
    async def call_tool(name: str, arguments: dict) -> list[types.TextContent]:
        # The condition gate lives here as well as in list_tools: a client
        # that calls a name we never advertised must not reach the tool.
        if name not in exposed:
            raise ValueError(f"Unknown tool for condition {condition}: {name}")
        log_tool_call(telemetry_log, name)
        # dispatch entries shell out to spelunk synchronously; off-thread so
        # a slow search can't stall the stdio event loop.
        result = await asyncio.to_thread(dispatch[name], arguments or {})
        return [types.TextContent(type="text", text=result)]

    async with stdio_server() as (read_stream, write_stream):
        await server.run(
            read_stream, write_stream, server.create_initialization_options()
        )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Bench-local stdio MCP server exposing agent.py's spelunk tools."
    )
    parser.add_argument(
        "--repo-path",
        required=True,
        help="Repo to run spelunk in. Explicit rather than inherited cwd — an "
        "MCP client's spawn cwd is not guaranteed.",
    )
    parser.add_argument("--condition", required=True, choices=list(SPELUNK_CONDITIONS))
    parser.add_argument(
        "--telemetry-log",
        default=None,
        help="Append one JSON line per tools/call here.",
    )
    args = parser.parse_args()

    repo_path = Path(args.repo_path).resolve()
    if not repo_path.is_dir():
        parser.error(f"repo-path does not exist: {repo_path}")

    telemetry_log = Path(args.telemetry_log) if args.telemetry_log else None
    asyncio.run(serve(args.condition, repo_path, telemetry_log))


if __name__ == "__main__":
    main()
