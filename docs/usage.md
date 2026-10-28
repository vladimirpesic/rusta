# Usage

Rusta has three front-ends over one agent: a **headless** run (`-p`), an
interactive **REPL**, and an optional **TUI**. With no subcommand, `rusta` runs
headless when given a prompt and opens the REPL otherwise.

- [Setup](#setup)
- [Headless runs](#headless-runs)
- [The REPL](#the-repl)
- [The terminal UI](#the-terminal-ui)
- [Slash commands](#slash-commands)
- [Sessions](#sessions)
- [Subcommands](#subcommands)

## Setup

```bash
rusta init
```

`init` asks which provider you use, which kind of key you hold, and for the key
itself; verifies the key against the endpoint before writing anything; then
saves `~/.config/rusta/config.toml` (mode `600`). Running `rusta` with no key
configured offers the same. It is scriptable — `echo "$KEY" | rusta init` — and
reads the key without echoing it when there is a terminal to echo on.

If the key is rejected, `init` offers the *other* plan rather than leaving you to
guess: the two kinds of key look identical, and each endpoint answers the
other's with an error that reads like a typo. A Kimi Coding Plan key is tried on
both [Coding hosts](providers.md#coding-plan-hosts), and the one that accepts it
is pinned. For Kimi it then prints the models the key can use, and caches the
list. Re-run it any time to switch provider or rotate a key; it edits the config
rather than replacing it.

Keys can also come from the environment alone:

```bash
export KIMI_API_KEY=sk-...       # Kimi Coding Plan (the default provider)
rusta --billing api -p "…"       # Kimi pay-per-token: kimi-k3.1-pro

export ZAI_API_KEY=...           # GLM / Z.AI
rusta --provider glm -p "…"

export DEEPSEEK_API_KEY=sk-...   # DeepSeek
rusta --provider deepseek -p "…"

export OPENAI_API_KEY=sk-...     # any OpenAI-compatible endpoint
rusta --provider openai -p "…"
```

`rusta doctor` checks the result: the provider, endpoint, key and model, the MCP
servers, and whether Devscriptor was found.

## Headless runs

The prompt can come from an argument, a file, or standard input — a long prompt
hits the shell's argument limit, and quoting one into `-p` mangles newlines:

```bash
rusta -p "Explain this repo" --yes      # inline
cat task.md | rusta -p --yes            # `-p` with no value reads stdin
rusta --prompt-file task.md --yes       # from a file (`-` means stdin)
```

`--yes` bypasses permission checks; without it, a call that needs approval is
asked about on the terminal (see [Permissions](permissions.md)).

**Background work.** If the model launches sub-agents (`agent`, `agent_swarm`)
and then finishes, a headless run holds the turn open until they reach a
terminal state and feeds their results back to the model, rather than exiting
and killing them. Only sub-agents hold the turn; a background shell command
started with `bash` is expected to outlive it. `Ctrl+C` interrupts the wait.
`--headless-background-mode` chooses the policy: `steer` (the default) also keeps
taking turns while goals, scheduled prompts or their notifications are pending,
`drain` waits for sub-agents without another model turn, and `exit` ends at
once. `--headless-wait-ceiling-s` and `--headless-max-turns` bound it.

**Exit status.** `0` on success; `1` when the prompt failed or a turn ran out of
steps (after the model's handoff); `4` when another process is writing the
session (see [Sessions](#sessions)).

**Terminal output.** What headless and REPL runs print to a terminal has its
control sequences stripped: the model's answer and thinking, tool-call lines,
tool output, and errors. A command's colours, a fetched page's escape codes, or
a `cat` of a binary file cannot drive your terminal. Each stream is filtered on
its own, so a sequence one tool leaves unfinished never holds back other output.
Output piped to a file or another program stays byte-exact.

### `--stream-json`

`--stream-json` prints one JSON object per line on stdout:

| Event | When |
| --- | --- |
| `assistant_delta`, `thinking_delta` | as the model streams its answer and its reasoning |
| `tool_call_delta` | as a tool call starts: its id and name |
| `tool_call` | the complete call: `arguments` and `status` (`pending` as it runs, `failed` when refused, or the result's own status for a repeat of a call in the same step, which shares its result) |
| `tool_result` | each call's result |
| `retry`, `compaction`, `stop` | when they happen |
| `response_id`, `usage` | at the end of the prompt |
| `approval_request` | a call that needs approval (`tool`, `arguments`, `reason`), without `--yes` |
| `question` | a question the model asks you |

A sub-agent's calls are `tool_call` and `tool_result` events too, each with the
sub-agent's `agent_id`. `usage` sums the tokens of every model step the prompt
took, its sub-agents' steps included (a background sub-agent's count in the
prompt during which it spent them); it is the same total a goal's token budget
counts, and it reports cache reads and writes as `input_cache_read` and
`input_cache_creation`.

An `approval_request` is answered with one line on stdin: `y` allow once, `a`
allow for the session, `n` deny, `s` deny for the session, `c` cancel. The
commands `bash` runs get none of that input: a command's stdin is the call's
`stdin`, or empty. A `question` is answered with one line. A failed prompt
prints an `error` event on stderr.

## The REPL

```bash
rusta                      # the configured provider
rusta --provider glm --yes
```

The REPL reads a line at a time with history (kept in
`~/.local/share/rusta/history`), streams each turn, and asks for approvals
inline. `Ctrl+C` interrupts the running turn; `/exit` or `/quit` leaves.

## The terminal UI

The TUI is behind a Cargo feature and is not built by default:

```bash
cargo build --release -p rusta-cli --features tui
rusta tui --provider glm --yes
```

The input box stays usable while a turn runs: `Enter` queues the input for
after the turn (firing the `UserPromptQueued` hook), `Ctrl-S` steers it into the
running turn at the next step boundary, and queued messages run in order once
the turn ends. A slash command or a shell-style line (`!…`) sent with `Ctrl-S`
is queued instead: a command runs between turns. Messages queued when the model
starts a `wait_for` are steered in automatically — unless they are shell-style
or slash commands — ending the wait early. `Esc` or `Ctrl+C` interrupts the turn
without quitting; a second `Ctrl+C` while idle exits, as does `Ctrl+D` on an
empty input (or `/exit`). Arrow keys scroll, responses stream as generated, and
an approval the turn asks for is answered with `y`/`n`/`a`/`s`/`c` while it
waits. Scheduled prompts that come due together share one turn: the first is its
prompt and the others are steered in.

The REPL reads a line at a time, so queueing and steering are TUI-only.
Interrupting a running turn works the same in both.

## Slash commands

The REPL and the TUI take these commands:

| Command | What it does |
| --- | --- |
| `/help` | List the commands |
| `/exit`, `/quit` | Leave |
| `/new` | Start a fresh conversation in the session, without its scheduled prompts |
| `/clear` | Clear the conversation context; scheduled prompts stay |
| `/undo [<count>]` | Remove the last user/assistant turn, or that many turns. The count must be a positive integer; a count past what can be undone is refused, not clamped |
| `/compact [<instruction>]` | Summarize the conversation now; the instruction is a focus hint for the summary |
| `/model [<id>]` | Switch model, or list the provider's models and mark the one in use. For Kimi and DeepSeek the list is the live one |
| `/provider <name>` | Switch provider |
| `/provider refresh [<name>]` | Fetch the provider's model list now rather than within a day; the session takes it at once. The reply names the models added and removed, and warns when the model in use is gone |
| `/permission <mode>`, `/auto`, `/yolo` | Change the permission mode (`/auto` is `accept_edits`, `/yolo` is `bypass_permissions`) |
| `/plan` | Turn plan mode on or off (see [Plan mode](permissions.md#plan-mode)) |
| `/swarm [on \| off \| <task>]` | Toggle swarm mode, or run one task in it (see [Agent swarm](agents.md#agent-swarm)) |
| `/tools` | List the tools the model is offered |
| `/mcp` | List the MCP servers and their state |
| `/reload` | Read profiles, skills and instruction files again and rebuild the system prompt |
| `/skill <name> [key=value …]` | Run a skill |
| `/web <query>` | Search the web with the session's search backend |
| `/goal create <objective> \| pause \| resume \| cancel` | Manage the session's goal |
| `/tasks` | List background tasks |
| `/sessions [<id>]` | List saved sessions, or show one |
| `/title <text>` | Name the session |
| `/add-dir <path>` | Add a directory the session may work in |
| `/takeover` | Take over a session another process holds (TUI) |

`/model` on the Kimi Coding Plan may ask you to confirm a switch first; see
[Confirmed model switches](providers.md#confirmed-model-switches).

`/reload` names what was added, changed or removed, and the model is told the
same in a reminder with the next message, so that it follows the current
versions; skills kept for the user are left out of that reminder. The `skill`,
`agent` and `agent_swarm` tools are replaced where they stand, which keeps the
tool list's order and so the prompt cache.

## Sessions

Every conversation is a session, recorded as JSONL in
`~/.local/share/rusta/records/<id>.jsonl` (the session is `default` unless
`--session` names one). `--session <ID> --resume` continues a saved session where
it stopped: its conversation, the tools it loaded with `select_tools`, its todo
list, goals and skills, and the files it has read, which it may overwrite
without reading them again unless they changed since. `--cwd <DIR>` (default:
the current directory) is the session's working directory: its tools run there,
and the project's config and instruction files are read from there.

**One process at a time.** Two Rusta processes on one session — two terminals on
the `default` session, a REPL and a `-p` run on the same `--session` — would
append to one records file, each unaware of the other's turns. So a session is
locked while a turn or a compaction writes it: a lock file beside its records
(`default.jsonl` has `default.lock`) names the process that holds it. Between
turns another process may use the session; the next turn here then finds the
records grown, reloads them, and goes on with the other's turns in its history.

- **When it is held,** a turn the lock refuses says which process holds the
  session (`rusta repl (pid 4242 on box)`). A headless run exits with code 4.
  The REPL asks to retry or cancel, and once a retry is refused too, offers to
  take the session over. The TUI puts the message back in the input, to send
  again, and `/takeover` takes the session over.
- **When the holder is gone** — its process has exited, is a zombie, or started
  before the last boot — the lock is taken over without asking.
- **Turning it off:** `RUSTA_SESSION_LOCK=0`. A lock that cannot be written,
  such as in a read-only data directory, lets the session run unlocked, with a
  warning.

The session index, `records/index.json`, is changed the same careful way: each
process reads it again before it writes it, under a short lock, so a session
another process started stays listed.

**Titles.** A session without a title gets one after a completed turn in the
REPL or the TUI, made from its first exchange:

- **Who writes it:** on the Kimi Coding Plan, the plan's own title service.
  Elsewhere, the session's provider with thinking off: `deepseek-flash` on
  DeepSeek, and on GLM-5.3, which cannot turn thinking off, its lightest
  thinking.
- **When it fails:** the title is made in the background, and a failure leaves
  the session untitled. It is tried again after a later turn, at most three times
  in a run.
- **Where it shows:** `rusta sessions list`, `/sessions`, the TUI's chat pane,
  and the hooks' `session_title`.

A title set with `/title`, or a fork's `fork of …`, is never replaced.
`auto_session_title = false` turns titling off, and headless runs make none.

## Subcommands

| Command | Purpose |
| --- | --- |
| `rusta init` | Set up a provider, plan, and API key, and save them to the config file |
| `rusta doctor` | Validate configuration, API keys, MCP servers, and whether Devscriptor was detected. For Kimi, names the host that accepts the key and lists its models. For GLM, checks the models against the Coding Plan and the vision server's environment. For DeepSeek, proves the key with a `GET /models`, lists the account's models and shows its balance. Reports which `.rusta.toml` settings workspace trust left out |
| `rusta version` | Print version information |
| `rusta completions <SHELL>` | Emit completions for `bash`, `elvish`, `fish`, `powershell`, `zsh` |
| `rusta sessions list [--all] [--cwd <DIR>] [--limit <N>] [--json]` | List saved sessions, most recent first: those that ran in the working directory (or `--cwd`), or every one with `--all` |
| `rusta fork [<ID>] [--as <NEW-ID>] [--cwd <DIR>] [-y]` | Copy a saved session, with its todo list, goals, skills and the files it has read, into a new one — by default the latest one that ran in the directory — and print the command that resumes it. The original is left as it was, and keeps its scheduled prompts |
| `rusta trust [-y] [--revoke] [--list]` | Show the settings in this folder's `.rusta.toml` that need trust, and trust the folder for them (`-y` without asking). `--revoke` forgets the folder, `--list` shows the trusted folders. See [Workspace trust](permissions.md#workspace-trust) |
| `rusta export --session <ID> --output <PATH>` | Export a session to a ZIP archive: its records, its sidecars and the large tool results its records refer to — nothing of other sessions |
| `rusta tui` | The terminal UI (requires the `tui` feature) |
