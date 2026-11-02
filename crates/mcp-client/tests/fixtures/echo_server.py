#!/usr/bin/env python3
"""Minimal MCP stdio server fixture exposing an `echo` tool, plus tools whose
results exercise every content shape a client must handle."""

import base64
import json
import os
import struct
import sys
import zlib


def tiny_png() -> str:
    """A 2x2 red PNG, base64-encoded."""
    raw = b"".join(b"\x00" + b"\xc8\x1e\x1e" * 2 for _ in range(2))

    def chunk(kind: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(kind + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", crc)

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )
    return base64.b64encode(png).decode()


# Results for the content-shape tools, by name.
SHAPED_RESULTS = {
    "structured_only": {"content": [], "structuredContent": {"answer": 42}},
    "structured_copy": {
        "content": [{"type": "text", "text": json.dumps({"answer": 42})}],
        "structuredContent": {"answer": 42},
    },
    "structured_extra": {
        "content": [{"type": "text", "text": "Found 1 result"}],
        "structuredContent": {"results": [{"id": 7}]},
    },
    "screenshot": {
        "content": [
            {"type": "text", "text": "Page captured"},
            {"type": "image", "data": tiny_png(), "mimeType": "image/png"},
        ]
    },
    "link": {
        "content": [
            {
                "type": "resource_link",
                "uri": "file:///tmp/report.pdf",
                "name": "report.pdf",
                "mimeType": "application/pdf",
            }
        ]
    },
    "sound": {"content": [{"type": "audio", "data": "UklGRg==", "mimeType": "audio/wav"}]},
    "blob": {
        "content": [
            {
                "type": "resource",
                "resource": {
                    "uri": "file:///data.bin",
                    "mimeType": "application/octet-stream",
                    "blob": "AAEC",
                },
            }
        ]
    },
}


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
                    "serverInfo": {"name": "echo-server", "version": "0.1.0"},
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
                            "name": "echo",
                            "description": "Echo the input message",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "message": {"type": "string"}
                                },
                                "required": ["message"],
                            },
                        },
                        {
                            "name": "cwd",
                            "description": "Report the server process's working directory",
                            "inputSchema": {"type": "object", "properties": {}},
                            "annotations": {"readOnlyHint": True},
                        },
                        {
                            "name": "fail",
                            "description": "Return a tool failure via isError, not a protocol error",
                            "inputSchema": {"type": "object", "properties": {}},
                        },
                        {
                            "name": "die",
                            "description": "Reply, then exit the process (simulate a mid-session crash)",
                            "inputSchema": {"type": "object", "properties": {}},
                            "annotations": {"readOnlyHint": False, "destructiveHint": True},
                        },
                    ]
                    + [
                        {
                            "name": name,
                            "description": f"Return a `{name}` shaped result",
                            "inputSchema": {"type": "object", "properties": {}},
                        }
                        for name in SHAPED_RESULTS
                    ]
                },
            }
        )
        return

    if method == "tools/call":
        params = req.get("params", {})
        name = params.get("name", "")
        arguments = params.get("arguments", {})
        if name == "echo":
            message = arguments.get("message", "")
            send(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "content": [{"type": "text", "text": f"echo: {message}"}],
                        "isError": False,
                    },
                }
            )
        elif name == "cwd":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "content": [{"type": "text", "text": os.getcwd()}],
                        "isError": False,
                    },
                }
            )
        elif name == "fail":
            # A *tool* failure: the JSON-RPC call succeeds, and the failure is
            # carried by `isError` on the result. This is how MCP servers report
            # a tool that ran and did not work.
            send(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "content": [
                            {"type": "text", "text": "file not found: /nope"}
                        ],
                        "isError": True,
                    },
                }
            )
        elif name in SHAPED_RESULTS:
            send({"jsonrpc": "2.0", "id": req_id, "result": SHAPED_RESULTS[name]})
        elif name == "die":
            # Acknowledge, flush, then terminate so the transport closes right
            # after a successful reply — exercising mid-session death + reconnect.
            send(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "content": [{"type": "text", "text": "dying"}],
                        "isError": False,
                    },
                }
            )
            sys.exit(0)
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {"code": -32601, "message": f"Unknown tool: {name}"},
                }
            )
        return

    if method == "ping":
        if req_id is not None:
            send({"jsonrpc": "2.0", "id": req_id, "result": {}})
        return

    if req_id is not None:
        send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32601, "message": f"Method not found: {method}"},
            }
        )


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as exc:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": None,
                    "error": {"code": -32700, "message": f"Parse error: {exc}"},
                }
            )
            continue
        handle(req)


if __name__ == "__main__":
    main()
