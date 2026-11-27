# Permissions and safety

Every tool call passes through Rusta's permission engine before it runs. This
page covers the permission modes and rules, the built-in guards, plan mode,
the write guard, and the trust a project's own config needs.

- [Permission modes](#permission-modes)
- [Rules](#rules)
- [Commands that only read](#commands-that-only-read)
- [Dangerous commands](#dangerous-commands)
- [Built-in guardrails](#built-in-guardrails)
- [Plan mode](#plan-mode)
- [Write safety](#write-safety)
- [What Rusta keeps on disk](#what-rusta-keeps-on-disk)
- [Workspace trust](#workspace-trust)

## Permission modes

| Mode | Behaviour |
| --- | --- |
| `default` | Reads run without asking, and so do [commands that only read](#commands-that-only-read); edits, other commands and MCP calls that change something are asked about |
| `accept_edits` | File edits run without asking; commands that do more than read are still asked about |
| `bypass_permissions` | Nothing is asked about, except the guardrails below and a plan review |

Set the mode with `--permission-mode` (`RUSTA_PERMISSION_MODE`,
`permission_mode`), `--yes` (bypass), or in a session with `/permission <mode>`,
`/auto` (`accept_edits`) and `/yolo` (`bypass_permissions`).

An approval prompt takes `y` (allow once), `a` (allow for the session), `n`
(deny), `s` (deny for the session) or `c` (cancel). An answer for the session
covers that call and no wider:

- **A `bash` command:** that command, run in the same `cwd`, with the same
  `stdin` and `env`. Allowing `cargo test` does not allow `cargo build`, nor
  `cargo test && rm -rf target`.
- **An MCP tool:** that tool alone. An MCP tool that runs commands or writes
  files is asked about like `bash` or the file tools, but an answer for it
  answers neither them nor another MCP tool.
- **`write_file` and `edit_file`:** every edit, as `accept_edits` gives.
- **A rule, a guardrail or a dangerous command:** what it covers, as said in
  their sections below.

A run with nobody to answer —
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

A rule's `name` is for you: a `prompt` rule's approval request names it and the
file it comes from (`asked by rule "ask-first" in project config
/repo/.rusta.toml`). An answer is remembered for the rule that asked and no
other: a "yes for the session" to one rule never answers for another rule, whatever the
names, nor for a built-in guardrail or a tool's own approval. Two rules of one
file may not share a name; Rusta refuses such a config file.

## Commands that only read

A `bash` command that can only read files inside the workspace runs without
asking, in every mode and in plan mode too, as `read_file` and `grep` do. It
also runs beside other reads instead of waiting for them. Anything Rusta cannot
judge with certainty is asked about as before:

- **Plain syntax.** Words, single quotes, double quotes without expansions,
  comments, `|`, `||`, `&&`, `;` and newlines. Outside single quotes a `$`, a
  backquote or a backslash makes it an ordinary command, and so does, outside
  any quotes, a glob (`*.rs`), braces, a `~` that would expand, a subshell, a
  background job, a here document or a variable assignment. Output may only
  be discarded (`2>/dev/null`, `>/dev/null`, `2>&1`), and input may be read
  from a file (`< file`).
- **Programs known to read:** `ls`, `cat`, `head`, `tail`, `wc`, `nl`, `cut`,
  `tr`, `sort`, `uniq`, `diff`, `cmp`, `comm`, `stat`, `du`, `df`, `file`,
  `tree`, `find`, `fd`, `grep`, `rg`, `sed`, `od`, `hexdump`, `xxd`,
  `strings`, the checksum programs, `echo`, `printf`, `pwd`, `cd` and a few
  more, each by its bare name. Options that write, run a program or follow
  links make the command an ordinary one: `sort -o`, `find -exec`,
  `find -delete`, `rg --pre`, `grep -R`, `diff -r`, `tree -o`, `uniq IN OUT`.
  `sed` counts only when its script prints (`sed -n '10,20p'`,
  `sed -n '/fn main/,/^}/p'`), never with `-i`, a script file, or a command
  that substitutes, reads or writes a file, or runs one.
- **Git's reading subcommands:** `status`, `log`, `show`, `diff`, `blame`,
  `grep`, `ls-files`, `rev-parse`, `describe`, branch and tag listings,
  `remote -v`, `stash list`, `worktree list` and the like, without
  `--output` or `--ext-diff`. They count only in a repository whose config sets
  nothing that runs a program or reads another work tree (an fsmonitor, a diff
  driver, a filter such as Git LFS's, a pager, an include, `core.worktree`),
  has no `post-index-change` hook, and has no submodules checked out.
- **Paths inside the workspace.** Each argument and option value is a path
  that must resolve inside the working directory or an allowed path, links
  followed, as the file tools' paths must. The exceptions are what a program
  takes as something else: a search pattern (unless it names an existing
  file), a `sed` script, or the text `echo` prints. After a `cd`, each path
  must stay inside both from the new directory and from the old one, since the
  `cd` may fail.
- **The session's environment.** A call that sets `env` is asked about. So is
  every command while `PATH` holds a relative directory, where a file in the
  workspace could stand in for a program. On Windows a path that starts at a
  root, names a drive or holds a backslash makes the command an ordinary one,
  since Git Bash reads such paths its own way.

Your rules come first, so a `deny` or `prompt` rule still applies to such a
command, and a command you refused for the session stays refused.

## Dangerous commands

In the `default` and `accept_edits` modes, `bash` asks before running a
dangerous command. The whole chain is checked, including command substitutions,
nested shells (`sh -c '…'`, `eval`) and the commands that run another:
`sudo`, `doas`, `env` (`-S` included), `command`, `exec`, `nohup`, `nice`,
`timeout`, `time`, `stdbuf`, `ionice`, `setsid`, `xargs` and `busybox`:

- **What counts as dangerous:** `rm -rf`, `shutdown`/`reboot`/`halt`/`poweroff`,
  `mkfs*`, `wipefs`, `dd` to a device, `systemctl poweroff|reboot|halt|kexec`, and
  `init 0|6`.
- **Unchecked commands:** a command whose name is only known at run time
  (`$CMD`), or whose quoting does not balance, is asked about too. A word that
  expands at run time stays in its place, so `sudo -u $USER rm -rf /` is still
  read as `rm -rf /`, and a wrapper whose next word could be an option or the
  command (`sudo $FLAGS …`) is asked about.
- **Exempt:** `rm -rf` whose operands are all literal paths under `/tmp` or
  `/temp`.
- **Rules and answers:** your own rules are checked first, so an `allow` rule can
  let a specific command through. Allowing one for the session allows that
  kind of command: `rm -rf`, but not `shutdown`.
- **Turning it off:** `dangerous_command_guard = false` (or
  `RUSTA_DANGEROUS_COMMAND_GUARD=0`), which turns off the built-in guardrails
  below too.

`bypass_permissions`, which `--yes` sets, never asks about these. A headless
`-p … --yes` run executes such commands without a prompt, so run it where that
is acceptable. A command's working directory is not restricted to the workspace
either, since the command itself can `cd` anywhere.

## Built-in guardrails

Two guardrails ask in every mode, `bypass_permissions` included, so a `--yes`
run still cannot write a secret or rewrite history unasked:

- **Secrets:** `write_file` and `edit_file` on `.env*`, `.ssh/`, `*.key`,
  `*.pem`, `.git/config`, `.git-credentials` and `id_*` files. A path is judged
  as given and as it resolves, the way the tool resolves it: a symlink named
  `notes.txt` that leads to `.env`, or a new file in a folder that is a symlink
  to `.ssh`, is asked about too.
- **Git history:** `git push`, `git reset`, `git rebase`, a forced
  `git checkout` or `git switch` (`-f`, `--force`, `--discard-changes`) and a
  forced `git clean`. Git is recognised however it is called: after its global
  options (`git -C dir push`, `git --git-dir=… reset`, `git -c key=value
  rebase`), quoted (`g''it push`), by path or as `git-push`, through the
  commands that run another (listed above), in a nested shell or `eval`, and in
  a command substitution. A git command whose operation cannot be read is asked
  about too: its subcommand or a force flag only known at run time, an option
  git does not document, or a `-c` that defines an alias. A command whose name
  is only known at run time (`$GIT push`) is not recognisably git, and in
  bypass mode it is not asked about.

The prompt says which guardrail asks and what it found (`destructive git
command: git push (built-in guardrail)`). Each guardrail's answer is its own:
a "yes for the session" to the secrets guardrail does not answer for the git one, and no
configured rule can answer for either. Your own rules are checked first, so an
`allow` rule lets a specific case through; `dangerous_command_guard = false`
turns both off. The REPL and the TUI ask in every mode, `--yes` included, when a
guardrail or a plan review applies.

## Keys stay with Rusta

No process Rusta starts sees the variables it reads its keys from. This covers
`bash` commands, background tasks, hooks, MCP servers and `git`. The variables
are:

- `RUSTA_API_KEY` and `RUSTA_ZAI_API_KEY`;
- `KIMI_API_KEY`, `ZAI_API_KEY`, `DEEPSEEK_API_KEY` and `OPENAI_API_KEY`;
- the variable named by the config's `api_key_env`;
- `RUSTA_CUSTOM_HEADERS`, whose headers may carry a token.

A command's output goes to the model and into the session records. Without
this, a command that printed its environment would send the key along with it.
Every other variable is passed on unchanged.

Rusta's own environment still holds the keys it was started with. On Linux, the
processes it starts cannot read them there either: Rusta's process is one the
kernel does not dump, so its `/proc/<pid>/environ`, memory and open files are
refused to every process without the privilege to trace any other. Rusta then
writes no core dump, and a debugger attaches to it only with that privilege. On
macOS and Windows, a process of the same user can still read another's
environment (`ps -E` on macOS).

A key given with `--api-key` is in Rusta's command line, which process listings
show: to the processes Rusta starts, and on Linux to every user. Rusta warns
when it is given one. `RUSTA_API_KEY`, or `api_key` in the config file, keeps
the key out of them, and `rusta --help` names `RUSTA_API_KEY` without showing
the key it holds.

None of this keeps a command from reading your files. Rusta runs commands as
you, without a sandbox, so a command can read the config file, or the shell
profile that sets a key, as you can. On Linux and macOS, the config file Rusta
writes when it sets up a key (`rusta init`) is readable by you alone: that keeps
other users out, not the commands you run.

An MCP server that needs one of these variables can be given it in its own
`env` ([MCP](mcp.md)), which applies on top.

## Plan mode

In plan mode the agent researches and writes a plan, and waits for your review
before it changes anything. Only you can turn it on: `/plan` in the REPL or the
TUI, `--plan` (or `RUSTA_PLAN`) at startup, or `plan = true` in the config file.
The agent has no tool to enter it, even when you ask it to in chat; it tells you
how instead.

- **While it is on:** every edit waits for your approval, in every permission
  mode and whatever `allow` rules say; `bash` asks too, unless
  `bypass_permissions` or an `allow` rule lets the command run, or the command
  [only reads](#commands-that-only-read); `cron_create` and
  `cron_delete` are refused. The model is reminded with each prompt, and a
  question about the code still gets a plain answer.
- **The review:** the agent presents the plan with `exit_plan_mode`, and you see
  the plan itself. `y` approves it and ends plan mode; `n` rejects it and plan
  mode stays on, so the agent revises it. Every plan is reviewed, in every
  permission mode, `--yes` included: an earlier "yes for the session" or an `allow` rule
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

Both tools replace a file whole, never part of it:

- **Atomically.** The new contents go to a temporary file in the same folder,
  with the file's own permissions, are flushed to disk, and are renamed over the
  file. A full disk, an I/O error or a crash leaves the old contents whole. A new
  file gets the permissions any new file gets.
- **Only if the file is still as read.** Right before the rename the file is
  checked again: one someone changed while the edit was made is left as they
  left it, and the tool says to read it again.
- **Symlinks stay links.** A symlink's target gets the new contents.
- **In place where a rename would change the file.** A file with other hard
  links, or owned by another user, is written in place after the same check, so
  every link sees the change and the owner stays; so is a file in a folder that
  does not let Rusta create the temporary file. Renaming also drops extended
  attributes and access lists set on the old file.

## What Rusta keeps on disk

The session records, their sidecars and blobs, the session index, spilled tool
output and attachments, background task output, schedules, goals, the input
history, the workspace trust store, cached provider data and the config `rusta
init` writes are yours alone on Linux and macOS: their folders are `0700` and
their files `0600`, whatever the umask. A file or folder of these that others
can read is made yours alone again when Rusta next writes it. `rusta export`
creates its archive `0600` too, in the folder you name. State files are replaced
atomically, so a crash leaves the old file or the new one.

On Windows, these files are in your profile (`%APPDATA%`, `%LOCALAPPDATA%`),
whose access list lets only you and administrators read them; Rusta sets no
access list of its own.

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
