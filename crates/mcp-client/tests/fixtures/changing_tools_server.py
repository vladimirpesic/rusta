#!/usr/bin/env python3
"""Minimal MCP stdio server fixture whose tool list changes mid-session.

It lists `keep` and `gone` at first. A call to `gone` answers "method not
found" and swaps `gone` for `fresh`, as a server that dropped a tool does,
so the client's rediscovery sees the new list."""

import json
import sys

STATE = {"tools": ["keep", "gone"]}


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
                    "serverInfo": {"name": "changing-tools-server", "version": "0.1.0"},
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
            for name in STATE["tools"]
        ]
        send({"jsonrpc": "2.0", "id": req_id, "result": {"tools": tools}})
        return

    if method == "tools/call":
        name = req.get("params", {}).get("name", "")
        if name not in STATE["tools"] or name == "gone":
            STATE["tools"] = ["keep", "fresh"]
            send(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {"code": -32601, "message": f"tool not found: {name}"},
                }
            )
            return
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
            print(f"changing_tools_server: {exc}", file=sys.stderr)


if __name__ == "__main__":
    main()
