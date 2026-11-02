#!/usr/bin/env python3
"""Drive the `rusta` REPL in a pseudo-terminal against mock_llm.py and
report, as one JSON object on stdout, when each marker reached the screen.

usage: repl_driver.py <rusta> <base-url> <home> <workdir>

Each line goes to the terminal in a single write, as a paste, typeahead or an
input method's word arrives: a prompt (marked `answer` once the mock's
answer shows), then `/help` (marked `help`), then `/exit` (marked `exited`
once the REPL has ended). Times are seconds since the first line was
written. A marker that never appears is absent from `marks`.
"""
import json
import os
import pty
import re
import select
import sys
import time

binary, base_url, home, workdir = sys.argv[1:5]
binary = os.path.abspath(binary)  # the child changes directory before exec
env = {
    "HOME": home,
    "XDG_CONFIG_HOME": os.path.join(home, ".config"),
    "XDG_DATA_HOME": os.path.join(home, ".local/share"),
    "XDG_CACHE_HOME": os.path.join(home, ".cache"),
    "PATH": "/usr/bin:/bin",
    "TERM": "xterm-256color",
}
args = [binary, "--provider", "kimi", "--base-url", base_url, "--api-key", "sk-test",
        "--model", "mock", "--session", "repl-pty"]

pid, fd = pty.fork()
if pid == 0:
    os.chdir(workdir)
    os.execve(binary, args, env)

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


def wait_for(label, pattern, timeout):
    """Pump until `pattern` is on screen; record when, relative to the start."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if re.search(pattern.encode(), screen):
            marks[label] = round(time.time() - origin, 2)
            return True
        if not pump(0.1):
            return False
    return False


pump(2.0)
origin = time.time()
os.write(fd, b"hello there\r")
if wait_for("answer", r"MOCK-DONE-1", 20):
    os.write(fd, b"/help\r")
    if wait_for("help", r"Slash commands", 10):
        os.write(fd, b"/exit\r")
        deadline = time.time() + 10
        while time.time() < deadline:
            pump(0.2)
            if os.waitpid(pid, os.WNOHANG) != (0, 0):
                marks["exited"] = round(time.time() - origin, 2)
                break
if "exited" not in marks:
    os.kill(pid, 9)
    os.waitpid(pid, 0)
print(json.dumps({"marks": marks}))
