# Hooks

Hooks run a command of yours when something happens in a session: intercept a
dangerous shell command before it runs, pop a notification when a background
task finishes, append the current git branch to every prompt.

```toml
# ~/.config/rusta/config.toml — the user config only (see below)
[[hooks]]
event = "PreToolUse"                   # when: one of the events below
matcher = "^bash$"                     # which targets: a regex over the event's input
command = "echo no bash 1>&2; exit 2"  # what to run
timeout = 5                            # seconds before it is killed (1–600, default 30)
```

When an event fires, every matching rule's command runs **in the project
directory**, in parallel (identical commands run once), with a snake_case JSON
payload on stdin:

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "01J…",
  "session_title": "Fix the login page",
  "client_type": "rusta_cli",
  "cwd": "/path/to/project",
  "tool_name": "bash",
  "tool_input": { "command": "rm -rf build" }
}
```

`session_title` is the session's title as it stands, empty while it has none: one
on record, one set with `/title`, or one made after a turn.

## Verdicts

The verdict comes from the exit code and stdout:

| Exit | Meaning | Rusta's behaviour |
| --- | --- | --- |
| `0` | allow | the operation continues; stdout may join the context |
| `2` | block | the operation stops, with stderr as the reason |
| anything else, a crash, or a timeout | script error | **fails open** — your work is never blocked by a broken hook |

A JSON deny on stdout blocks too:
`{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"…"}}`.

Only **PreToolUse**, **UserPromptSubmit** and **Stop** can block; every other
event is observation-only. Because hooks fail open, they are fine for alerts and
lightweight interception, but not as the sole guard of a high-risk operation —
use [permission rules](permissions.md) for that.

- **`UserPromptSubmit`** fires for your own prompts only, not for a scheduled or
  continuation turn. Its stdout is folded into the prompt message itself; a
  blocked prompt never reaches the model.
- **`TurnStarted`** reports each turn's `origin_kind`: `user`, `cron_job` for a
  scheduled prompt the TUI runs as its own turn, or `system_trigger`, with
  `origin_name` `headless_continuation`, for a headless run's follow-up turn.
- **`PreToolUse`** runs before the permission check: a block refuses the call
  before any approval, so you are not asked about a call that will not run, and
  `PermissionRequest` / `PermissionResult` do not fire for it.
- **`Stop`** runs once the turn is really ending — not for a step that steered
  input or a tool call keeps going, and not for a filtered answer. A block (exit
  2, or a JSON deny) hands its reason back to the model, which continues the
  turn, at most once per turn. Exit 0 ends the turn and its stdout is ignored, so
  a hook that only notifies never sends the turn back to the model. The payload
  carries `stop_hook_active` (`false` when the hook runs).

A hook's verdict is its exit: a process it leaves running in the background can
hold its output open, but Rusta reads on for only 200 ms after the hook exits.
Each output stream is kept to its first 100,000 characters.

On non-Windows platforms a hook runs in its own process group; on timeout it
gets SIGTERM, a short grace, then SIGKILL. `SessionEnd` runs once however the
session ends: a finished run, a failed or interrupted one, one that ran out of
steps, or one whose prompt could not be read. While `SessionEnd` hooks run at
shutdown, Rusta shows which hook is running, and a second `Ctrl+C` exits without
waiting for them.

## Events

The matcher regex tests the input in the second column.

| Event | Matcher input | Blocks? |
| --- | --- | --- |
| `UserPromptSubmit` | the submitted text | ✓ |
| `PreToolUse` | tool name | ✓ |
| `Stop` | (empty) | ✓ |
| `UserPromptQueued` | the queued text; fires when the TUI queues a prompt for after the running turn | — |
| `TurnStarted` | turn origin (`user`, `cron_job`, `system_trigger`) | — |
| `PostToolUse` / `PostToolUseFailure` | tool name | — |
| `PermissionRequest` / `PermissionResult` | tool name | — |
| `SessionStart` / `SessionEnd` | `startup` / `resume`, `exit` | — |
| `SessionHeartbeat` | (empty); fires every 60 s while configured | — |
| `SubagentStart` / `SubagentStop` | sub-agent name | — |
| `TaskStarted` | task kind (`agent`, `process`) | — |
| `StopFailure` | the error kind | — |
| `Interrupt` | (empty); the user cut the turn short | — |
| `PreCompact` / `PostCompact` | `manual` / `auto` | — |
| `Notification` | e.g. `task.completed` | — |

## User config only

A project `.rusta.toml` that declares `[[hooks]]` is refused outright, with a
clear message: hooks run code, and a cloned repository must not run code on the
machine that checked it out. Put hooks in `~/.config/rusta/config.toml` (or pass
`--config`).
