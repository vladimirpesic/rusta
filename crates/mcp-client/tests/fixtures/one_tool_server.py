#!/usr/bin/env python3
"""Minimal MCP stdio server fixture exposing a single tool, for exercising
the deferral decision on a server below DEFER_TOOL_THRESHOLD."""

import json
import sys


def send(msg: dict) -> None:
    line = json.dumps(msg, separators=(",", ":"))
    sys.stdout.write(line + "\n")
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
                    "serverInfo": {"name": "one-tool-server", "version": "0.1.0"},
                    "capabilities": {"tools": {}},
                },
            }
        )
        return

    if method == "notifications/initialized":
        return

    if method == "tools/list":
        send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": [
                        {
                            "name": "ping",
                            "description": "Answer pong",
                            "inputSchema": {"type": "object", "properties": {}},
                        }
                    ]
                },
            }
        )
        return

    if method == "tools/call":
        send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {"content": [{"type": "text", "text": "pong"}]},
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
            print(f"one_tool_server: {exc}", file=sys.stderr)


if __name__ == "__main__":
    main()
