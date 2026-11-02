# Permissions and safety

Every tool call passes through Rusta's permission engine before it runs. This
page covers the permission modes and rules, the built-in guards, plan mode,
the write guard, and the trust a project's own config needs.

- [Permission modes](#permission-modes)
- [Rules](#rules)
- [Dangerous commands](#dangerous-commands)
- [Built-in guardrails](#built-in-guardrails)
- [Plan mode](#plan-mode)
- [Write safety](#write-safety)
- [Workspace trust](#workspace-trust)

## Permission modes

| Mode | Behaviour |
| --- | --- |
| `default` | Reads run without asking; edits, commands and MCP calls that change something are asked about |
| `accept_edits` | File edits run without asking; commands are still asked about |
| `bypass_permissions` | Nothing is asked about, except the guardrails below and a plan review |

Set the mode with `--permission-mode` (`RUSTA_PERMISSION_MODE`,
`permission_mode`), `--yes` (bypass), or in a session with `/permission <mode>`,
`/auto` (`accept_edits`) and `/yolo` (`bypass_permissions`).

An approval prompt takes `y` (allow once), `a` (allow for the session), `n`
(deny), `s` (deny for the session) or `c` (cancel). A run with nobody to answer —
a headless `-p … --yes` run, a background sub-agent — refuses what would be asked
at once, and tells the model that nobody could approve the call rather than that
you refused it.

A sub-agent is held to the session's permission mode and rules, and what needs
approval is asked through the session, naming the sub-agent (`sub-agent sub-…:`,
or `agent_id` in a `--stream-json` `approval_request`).

## Rules

Rules in `[[permissions.rules]]` are checked before the mode; the first matching
rule wins:

```toml
[[permissions.rules]]
name = "allow-cargo"
tool = "bash"
command = "^cargo (build|test|check)"
action = "allow"      # allow | deny | prompt

[[permissions.rules]]
name = "no-secrets"
tool = "{write_file,edit_file}"
path = "**/secrets/**"
action = "deny"
```

`tool` and `path` are globs; `command` is a regex. A compound command is matched
**per segment** (split on `&&`, `||`, `;`, `|`, newline): an `allow` must match
*every* segment, so a rule permitting `git status` does not also permit
`git status && rm -rf /`; a `deny` or `prompt` fires if *any* segment matches, so
a guardrail still catches the dangerous half of a chain. A rule's `scope` says
how long the decision on a call it matches is remembered: `once` (the default),
`session`, or `always`, which lasts the session too, since decisions are not
kept across restarts.

## Dangerous commands

In the `default` and `accept_edits` modes, `bash` asks before running a
dangerous command. The whole chain is checked, including `sudo …`, `sh -c '…'`,
`eval` and command substitutions:

- **What counts as dangerous:** `rm -rf`, `shutdown`/`reboot`/`halt`/`poweroff`,
  `mkfs*`, `wipefs`, `dd` to a device, `systemctl poweroff|reboot|halt|kexec`, and
  `init 0|6`.
- **Unchecked commands:** a command whose name is only known at run time
  (`$CMD`), or whose quoting does not balance, is asked about too.
- **Exempt:** `rm -rf` whose operands are all literal paths under `/tmp` or
  `/temp`.
- **Rules and answers:** your own rules are checked first, so an `allow` rule can
  let a specific command through. Answering "yes to all" allows that kind of
  command for the session: `rm -rf`, but not `shutdown`.
- **Turning it off:** `dangerous_command_guard = false` (or
  `RUSTA_DANGEROUS_COMMAND_GUARD=0`), which turns off the built-in guardrails
  below too.

`bypass_permissions`, which `--yes` sets, never asks about these. A headless
`-p … --yes` run executes such commands without a prompt, so run it where that
is acceptable. A command's working directory is not restricted to the workspace
either, since the command itself can `cd` anywhere.

## Built-in guardrails

Two rules ask in every mode, `bypass_permissions` included, so a `--yes` run
still cannot write a secret or rewrite history unasked:

- `write_file` and `edit_file` on `.env*`, `.ssh/`, `*.key`, `*.pem`,
  `.git/config`, `.git-credentials` and `id_*` files;
- `git push`, `git reset`, `git rebase`, `git checkout -f` and `git clean -f`.

Your own rules are checked first, so an `allow` rule lets a specific case
through; `dangerous_command_guard = false` turns both off. The REPL and the TUI
ask in every mode, `--yes` included, when a guardrail or a plan review applies.

## Plan mode

In plan mode the agent researches and writes a plan, and waits for your review
before it changes anything. Only you can turn it on: `/plan` in the REPL or the
TUI, `--plan` (or `RUSTA_PLAN`) at startup, or `plan = true` in the config file.
The agent has no tool to enter it, even when you ask it to in chat; it tells you
how instead.

- **While it is on:** every edit waits for your approval, in every permission
  mode and whatever `allow` rules say; `bash` asks too, unless
  `bypass_permissions` or an `allow` rule lets the command run; `cron_create` and
  `cron_delete` are refused. The model is reminded with each prompt, and a
  question about the code still gets a plain answer.
- **The review:** the agent presents the plan with `exit_plan_mode`, and you see
  the plan itself. `y` approves it and ends plan mode; `n` rejects it and plan
  mode stays on, so the agent revises it. Every plan is reviewed, in every
  permission mode, `--yes` included: an earlier "yes to all" or an `allow` rule
  does not approve the next one, though a `deny` rule refuses them all. With
  `--stream-json` the review is an `approval_request` for `exit_plan_mode`, with
  the plan in `arguments.plan`.
- **Turning it off:** `/plan` again. The agent is told that plan mode ended, and
  carries out a plan only if you approved one.
- **Headless runs:** a `-p` run without `--yes` reviews the plan on stdin like any
  approval. With `--yes` nobody can review it: `plan = true` from the config file
  is not applied, and an explicit `--plan` keeps plan mode on to the end of the
  run, with the plan unapproved and nothing edited.
- **Sub-agents** are never in plan mode, and do not get `exit_plan_mode`.

## Write safety

`write_file` replaces a file wholesale, from a picture of it the model formed
earlier. So it refuses to overwrite a file this agent has not read, or one that
changed on disk since it read it — a human edited it, a formatter ran, another
agent wrote it — and names the remedy ("read it again with read_file, then
retry"). Creating a new file needs no read.

`edit_file` needs no prior read: its `old_string` must match the file as it is,
exactly once, so the edit itself shows the model knows the text it changes. An
edit re-stamps a file the agent read and that has not changed since, so a run of
edits and writes proceeds; of any other file the agent knows only the text it
replaced, and a later overwrite needs a read first.

- **How staleness is judged:** length, modification time **and** a content
  digest. The first two miss an equal-length rewrite inside one clock tick, which
  is precisely the shape of a formatter reflowing a line. Files over 8 MiB are
  judged on length and modification time alone, to keep a write from re-reading
  a large artefact.
- **Per agent:** a sub-agent starts with an empty record and must read a file
  itself, since it does not share the parent's view of the workspace.
- **Across resumes:** the record is saved beside the session's records, so a
  resumed session may overwrite the files it read before, unless they changed
  since.

`edit_file` replaces text as `read_file` shows it:

- **Exactly one match.** `old_string` must occur once, or the edit is refused
  with the count; `replace_all` changes every occurrence instead. An empty
  `old_string`, or one equal to `new_string`, is refused too.
- **Line endings.** The match is made in the read view, where CRLF shows as LF,
  and the file keeps its own line endings: a CRLF file stays CRLF, and in a file
  that mixes the two, new line breaks follow the line the match starts on.
- **Copied line numbers.** When `old_string` is not found and every line of it
  starts with `read_file`'s numbers, counting up, the numbers were copied along.
  They are dropped from both strings, the text is matched on the line they name,
  even with its indentation left out (or wherever it occurs once), and the result
  says so.

## Workspace trust

`.rusta.toml` comes with the repository, so a cloned repository could use it to
run commands or redirect your key. Settings that reach beyond the project
therefore apply only in a folder you trust:

- **Commands and connections:** `[[mcp_servers]]` entries, which run a command or
  send tool calls to a URL. An entry with `disabled = true` applies anyway, since
  it can only switch a server off.
- **Where requests go:** `base_url`, `api_key`, `api_key_env`, `custom_headers`,
  `search_url`, and `records` (where the conversation is written).
- **Looser permissions:** `allow` rules, a `permission_mode` looser than your own
  config's, and `dangerous_command_guard = false`. `deny` and `prompt` rules, a
  stricter mode and `dangerous_command_guard = true` apply anyway.
- **Paths outside the folder:** `allowed_paths`, `additional_dirs`,
  `project_context_files` and `skill_dirs` entries that lead out of it, by `..`,
  `~`, an absolute path or a symlink. Entries inside it apply anyway.

Everything else in the file (model, effort, turn limits and so on) applies as
usual. Your own config, `--config` and `RUSTA_CONFIG` are never gated.

- **Interactive sessions** (the REPL and the TUI) lead with a summary of what
  trusting activates — how many MCP servers start and from which config, how
  many folders outside the project are reached, how many files outside it are
  read into the prompt, which project instructions and agent profiles load — and
  then list those settings exactly and ask: `t` ("Trust and continue") trusts the
  folder and remembers it, `o` applies them for this run only, and anything else
  (`e`, "Exit", or just Enter) leaves them out.
- **Changes.** Trust covers exactly the settings shown — the prompt says so — not
  the folder: if any of them changes, for example after a `git pull`, you are
  asked again.
- **Headless runs** (`-p`) never ask. They leave the settings out and print what
  was left out on stderr.
- **`RUSTA_TRUST_WORKSPACE=1`** (`--trust-workspace`) applies them for one run
  without asking or remembering. `RUSTA_TRUST_WORKSPACE=0` leaves them out even
  in a trusted folder.
- **`rusta trust`** shows the settings and trusts the folder for them, which is
  how to set up a folder for headless runs. `-y` skips the question, `--revoke`
  forgets the folder, and `--list` shows the trusted folders.
- **`rusta doctor`** reports what applies and what was left out.

Trusted folders are recorded in `trusted-folders.json` in Rusta's data directory
(`~/.local/share/rusta/` on Linux).

Instruction files (`AGENTS.md`), skills and profiles inside the project are read
without trust, like the rest of the repository's contents: they are text for the
model, and every tool call they lead to still goes through your permissions.
