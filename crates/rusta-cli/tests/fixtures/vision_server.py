#!/usr/bin/env python3
"""Minimal MCP stdio server fixture standing in for Z.AI's vision server.

Its `analyze_image` validates arguments as `@z_ai/mcp-server` 0.1.5 does:
`image_source` is a local `.png`/`.jpg`/`.jpeg` file or an http(s) URL, and
`prompt` is required. A valid call answers
`analyzed <path> (<bytes> bytes): <prompt>`; an invalid one is a tool error
(`isError`), as the real server reports it."""

import json
import os
import sys

TOOLS = [
    {
        "name": "ui_diff_check",
        "description": "Compare two UI screenshots",
        "inputSchema": {
            "type": "object",
            "properties": {
                "expected_image_source": {"type": "string"},
                "actual_image_source": {"type": "string"},
                "prompt": {"type": "string"},
            },
        },
    },
    {
        "name": "analyze_image",
        "description": "General-purpose image analysis",
        "inputSchema": {
            "type": "object",
            "properties": {"image_source": {"type": "string"}, "prompt": {"type": "string"}},
            "required": ["image_source", "prompt"],
        },
    },
]


def send(msg: dict) -> None:
    sys.stdout.write(json.dumps(msg, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def result(req_id, text: str, is_error: bool = False) -> None:
    send(
        {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {"content": [{"type": "text", "text": text}], "isError": is_error},
        }
    )


def analyze(req_id, args: dict) -> None:
    source = args.get("image_source") or ""
    prompt = args.get("prompt") or ""
    if not source or not prompt:
        result(req_id, "Validation failed: image_source and prompt are required", True)
        return
    if source.startswith(("http://", "https://")):
        result(req_id, f"analyzed {source}: {prompt}")
        return
    if not os.path.exists(source):
        result(req_id, f"Image file not found: {source[:40]}", True)
        return
    ext = os.path.splitext(source)[1].lower()
    if ext not in (".png", ".jpg", ".jpeg"):
        result(req_id, f"Unsupported image format: {ext}", True)
        return
    result(req_id, f"analyzed {source} ({os.path.getsize(source)} bytes): {prompt}")


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
                    "serverInfo": {"name": "vision-server", "version": "0.1.0"},
                    "capabilities": {"tools": {}},
                },
            }
        )
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req_id, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        params = req.get("params", {})
        if params.get("name") == "analyze_image":
            analyze(req_id, params.get("arguments") or {})
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {"code": -32601, "message": f"tool not found: {params.get('name')}"},
                }
            )
    elif req_id is not None:
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
            print(f"vision_server: {exc}", file=sys.stderr)


if __name__ == "__main__":
    main()
