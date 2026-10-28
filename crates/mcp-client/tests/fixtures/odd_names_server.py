#!/usr/bin/env python3
"""Minimal MCP stdio server fixture whose tool names model APIs refuse as
they are: a dot, a space, and two long names sharing an 80-character
prefix. Every call answers `called <tool name>`, so a test can tell which
tool a qualified name reached."""

import json
import sys

LONG = "x" * 80
TOOLS = ["read.file", "read file", LONG + "_alpha", LONG + "_beta", "plain_tool"]


def send(msg: dict) -> None:
    sys.stdout.write(json.dumps(msg, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def handle(req: dict) -> None:
    method = req.get("method")
    req_id = req.get("id")

    if method == "initialize":
        send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {"name": "odd-names-server", "version": "0.1.0"},
                    "capabilities": {"tools": {}},
                },
            }
        )
        return

    if method == "notifications/initialized":
        return

    if method == "tools/list":
        tools = [
            {
                "name": name,
                "description": f"Tool {name}",
                "inputSchema": {"type": "object", "properties": {}},
            }
            for name in TOOLS
        ]
        send({"jsonrpc": "2.0", "id": req_id, "result": {"tools": tools}})
        return

    if method == "tools/call":
        name = req.get("params", {}).get("name", "")
        send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {"content": [{"type": "text", "text": f"called {name}"}]},
            }
        )
        return

    if req_id is not None:
        send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32601, "message": f"method not found: {method}"},
            }
        )


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            handle(json.loads(line))
        except Exception as exc:  # keep the fixture alive on a bad frame
            print(f"odd_names_server: {exc}", file=sys.stderr)


if __name__ == "__main__":
    main()
