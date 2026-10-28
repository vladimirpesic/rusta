#!/usr/bin/env python3
"""A mock OpenAI-compatible chat-completions endpoint for the TUI pty tests.

usage: mock_llm.py <mode> <request-log> [chunk-delay-seconds]

Binds a free local port and prints `PORT <n>` on stdout. Every request is
numbered from 1 and logged to <request-log> as one JSON line, with each
message's role and text. Every response is a stream of `STREAM-<n>-<i> `
chunks, `<delay>` seconds apart, so a screen can tell the requests apart:

- `text`: five chunks, then `MOCK-DONE-<n>`.
- `tool`: five chunks, then a `bash` call (which needs approval in the
  default permission mode); the request carrying its result answers
  `MOCK-DONE-<n>` at once.

`POST /tools` is the Kimi Coding Plan's title service, which titles the
session after its first turn: it answers `Mock session`, and is neither
numbered nor logged.
"""
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODE = sys.argv[1]
LOG_PATH = sys.argv[2]
DELAY = float(sys.argv[3]) if len(sys.argv) > 3 else 0.5
COUNTER = {"n": 0}
LOCK = threading.Lock()


def chunk(delta, finish=None):
    return {
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(part.get("text", "") for part in content if isinstance(part, dict))
    return ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass

    def send_sse(self, payloads, delay):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("connection", "close")
        self.end_headers()
        try:
            for payload in payloads:
                self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())
                self.wfile.flush()
                time.sleep(delay)
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # the client cancelled the turn
        self.close_connection = True

    def do_GET(self):
        self.send_json({"object": "list", "data": [{"id": "mock", "object": "model"}]})

    def send_json(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        if self.path.endswith("/tools"):
            self.send_json({"title": "Mock session"})
            return
        messages = body.get("messages", [])
        with LOCK:
            COUNTER["n"] += 1
            n = COUNTER["n"]
            with open(LOG_PATH, "a", encoding="utf-8") as log:
                record = {"n": n, "messages": [
                    {"role": m.get("role"), "text": text_of(m.get("content"))} for m in messages
                ]}
                log.write(json.dumps(record) + "\n")
        has_tool_result = any(m.get("role") == "tool" for m in messages)
        stream = [chunk({"role": "assistant", "content": ""})]
        stream += [chunk({"content": f"STREAM-{n}-{i} "}) for i in range(5)]
        if MODE == "tool" and has_tool_result:
            self.send_sse([chunk({"role": "assistant", "content": f"MOCK-DONE-{n}"}), chunk({}, "stop")], 0.0)
        elif MODE == "tool":
            call = {
                "index": 0,
                "id": f"call_mock_{n}",
                "type": "function",
                "function": {"name": "bash", "arguments": json.dumps({"command": "echo approved-run"})},
            }
            self.send_sse(stream + [chunk({"tool_calls": [call]}), chunk({}, "tool_calls")], DELAY)
        else:
            self.send_sse(stream + [chunk({"content": f"MOCK-DONE-{n}"}), chunk({}, "stop")], DELAY)


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(f"PORT {server.server_address[1]}", flush=True)
    server.serve_forever()
