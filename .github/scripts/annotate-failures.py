"""Report a failed cargo step as GitHub Actions error annotations.

Usage: annotate-failures.py <title> <log>

Annotations stay readable after the run, in the job's summary and through
the checks API, without downloading the log. The compiler errors and the
failed tests' output fill them, several to one, and a last one lists the
failed tests and targets; a log with none of them is reported by its last
lines.
"""

import re
import sys

# A step shows at most 10 error annotations: 9 of details and the summary.
MAX_DETAILS = 9
MAX_CHARS = 4000
MAX_DETAIL_CHARS = 1500

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
COMPILER_ERROR = re.compile(r"^error(\[E\d+\])?: ")
NOT_A_DIAGNOSTIC = re.compile(
    r"^error: (could not compile|test failed|\d+ targets? failed|aborting|build failed)"
)
TEST_OUTPUT = re.compile(r"^---- (.+) (stdout|stderr) ----$")
FAILED_TEST = re.compile(r"^test (.+) \.\.\. FAILED$")
BACKTRACE_FRAME = re.compile(r"^\s+(\d+: |at )")


def escape(text):
    text = text if len(text) <= MAX_CHARS else text[:MAX_CHARS] + "\n[...]"
    return text.replace("%", "%25").replace("\r", "").replace("\n", "%0A")


def escape_property(text):
    return escape(text).replace(":", "%3A").replace(",", "%2C")


def annotate(title, message):
    print(f"::error title={escape_property(title)}::{escape(message)}")


def without_backtraces(lines):
    """`lines` without the frames of the backtraces in them."""
    kept, frames = [], False
    for line in lines:
        if line.strip() == "stack backtrace:":
            frames = True
            kept.append("stack backtrace: [left out]")
        elif not (frames and BACKTRACE_FRAME.match(line)):
            frames = False
            kept.append(line)
    return kept


def details(lines):
    """The compiler errors and failed tests' output, in log order."""
    found = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if COMPILER_ERROR.match(line) and not NOT_A_DIAGNOSTIC.match(line):
            end = i + 1
            while end < len(lines) and lines[end].strip():
                end += 1
            found.append(("\n".join(lines[i:end]), line))
            i = end
        elif TEST_OUTPUT.match(line):
            end = i + 1
            while end < len(lines) and not (
                TEST_OUTPUT.match(lines[end]) or lines[end] == "failures:"
            ):
                end += 1
            found.append(("\n".join(without_backtraces(lines[i:end])).strip(), line))
            i = end
        else:
            i += 1
    return found


def packed(found):
    """The texts of `found`, each cut to MAX_DETAIL_CHARS, in annotations of at
    most MAX_CHARS each, in log order; and the first lines of those left out."""
    packs, current, left_out = [], "", []
    for text, first in found:
        if len(text) > MAX_DETAIL_CHARS:
            text = text[:MAX_DETAIL_CHARS] + "\n[...]"
        if current and len(current) + 2 + len(text) > MAX_CHARS:
            packs.append(current)
            current = ""
        if len(packs) == MAX_DETAILS:
            left_out.append(first)
            continue
        current = f"{current}\n\n{text}" if current else text
    if current and len(packs) < MAX_DETAILS:
        packs.append(current)
    return packs, left_out


def main():
    # Windows' console encoding cannot hold everything a test prints.
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    title, path = sys.argv[1], sys.argv[2]
    with open(path, encoding="utf-8", errors="replace") as log:
        lines = [ANSI.sub("", line.rstrip("\n")) for line in log]

    found = details(lines)
    packs, left_out = packed(found)
    for text in packs:
        annotate(title, text)

    failed = [m.group(1) for m in map(FAILED_TEST.match, lines) if m]
    targets = [line.strip() for line in lines if line.startswith("    `-p ")]
    summary = []
    if failed:
        summary.append(f"{len(failed)} failed tests:")
        summary += [f"  {name}" for name in failed]
    if targets:
        summary.append("Failed targets:")
        summary += [f"  {target}" for target in targets]
    if left_out:
        summary.append(f"{len(left_out)} more errors and failures:")
        summary += [f"  {first}" for first in left_out]
    if not found:
        summary.append("Last lines of the log:")
        summary += lines[-60:]
    if summary:
        annotate(f"{title}: summary", "\n".join(summary))


if __name__ == "__main__":
    main()
