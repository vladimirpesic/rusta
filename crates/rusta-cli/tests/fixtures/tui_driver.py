#!/usr/bin/env python3
"""Drive `rusta tui` in a pseudo-terminal against mock_llm.py and report,
as one JSON object on stdout, when each marker reached the screen.

usage: tui_driver.py <rusta> <base-url> <home> <workdir> <scenario>

Scenarios:
- `stream`: send a prompt; mark the first and last streamed chunk, the
  final answer, and the session title made after the turn on the chat
  pane's border.
- `approval`: send a prompt whose answer calls `bash`; mark the approval
  prompt, answer `y`, mark the final answer.
- `keys`: while the first turn streams, Enter-queue `queued-one` and
  Ctrl-S-steer `steer-one`; once the queued prompt's own turn streams,
  press Esc; mark each acknowledgement, and whether the TUI is still running
  after the interrupt.

Times are seconds since the prompt's Enter. A marker that never appears is
absent from `marks`.
"""
import fcntl
import json
import os
import pty
import re
import select
import struct
import sys
import termios
import time

binary, base_url, home, workdir, scenario = sys.argv[1:6]
binary = os.path.abspath(binary)  # the child changes directory before exec
env = {
    "HOME": home,
    "XDG_CONFIG_HOME": os.path.join(home, ".config"),
    "XDG_DATA_HOME": os.path.join(home, ".local/share"),
    "XDG_CACHE_HOME": os.path.join(home, ".cache"),
    "PATH": "/usr/bin:/bin",
    "TERM": "xterm-256color",
}
args = [binary, "--provider", "kimi", "--base-url", base_url, "--api-key", "sk-test", "--model", "mock", "tui"]

pid, fd = pty.fork()
if pid == 0:
    os.chdir(workdir)
    os.execve(binary, args, env)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))

screen = b""
origin = None
marks = {}


def pump(seconds):
    """Read what the terminal receives for `seconds`; False once it closed."""
    global screen
    end = time.time() + seconds
    while time.time() < end:
        if select.select([fd], [], [], 0.05)[0]:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return False
            if not data:
                return False
            screen += data
    return True


def seen(pattern):
    return re.search(pattern.encode(), screen) is not None


def wait_for(label, pattern, timeout):
    """Pump until `pattern` is on screen; record when, relative to Enter."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if seen(pattern):
            marks[label] = round(time.time() - origin, 2)
            return True
        if not pump(0.1):
            return False
    return False


def type_text(text):
    for ch in text:
        os.write(fd, ch.encode())
        pump(0.02)


def alive():
    return os.waitpid(pid, os.WNOHANG) == (0, 0)


pump(2.0)
type_text("hello")
os.write(fd, b"\r")
origin = time.time()

if scenario == "stream":
    wait_for("first_chunk", r"STREAM-1-0", 15)
    wait_for("last_chunk", r"STREAM-1-4", 15)
    wait_for("final_answer", r"MOCK-DONE-1", 15)
    # Only the cells that change are drawn: the title, not the "Chat" before it.
    wait_for("title", r"Mock session", 15)
elif scenario == "approval":
    if wait_for("approval_prompt", r"\[y/n/a/s/c\]", 20):
        os.write(fd, b"y")
        wait_for("final_answer", r"MOCK-DONE-2", 15)
elif scenario == "keys":
    if wait_for("first_chunk", r"STREAM-1-0", 15):
        type_text("queued-one")
        os.write(fd, b"\r")
        wait_for("queued", r"\[queued\] queued-one", 5)
        type_text("steer-one")
        os.write(fd, b"\x13")  # Ctrl-S
        wait_for("steered", r"\[steered\] steer-one", 5)
        # The steered message keeps the turn going (request 2); the queued
        # prompt then runs as a turn of its own (request 3).
        if wait_for("queued_turn_streams", r"STREAM-3-0", 30):
            os.write(fd, b"\x1b")  # Esc
            wait_for("interrupting", r"Interrupting", 5)
            pump(1.0)
            marks["alive_after_interrupt"] = alive()
else:
    sys.exit(f"unknown scenario {scenario!r}")

os.write(fd, b"\x03")
pump(0.3)
os.write(fd, b"\x03")
pump(1.0)
if alive():
    os.kill(pid, 9)
    os.waitpid(pid, 0)
print(json.dumps({"marks": marks}))
