# Tools

- [Built-in tools](#built-in-tools)
- [Files](#files)
- [Shell and background work](#shell-and-background-work)
- [Web tools](#web-tools)
- [Media](#media)
- [Scheduled prompts](#scheduled-prompts)
- [Todo list, goals and skills](#todo-list-goals-and-skills)
- [Output limits](#output-limits)

MCP tools, their naming and on-demand loading are covered in [MCP](mcp.md).

## Built-in tools

An agent has 26 built-in tools:

| Category | Tools |
| --- | --- |
| Filesystem | `read_file`, `write_file`, `edit_file`, `list_dir`, `glob`, `grep` |
| Execution | `bash` |
| Web | `fetch`, `web_search` |
| Planning | `todo_list`, `exit_plan_mode` (see [Plan mode](permissions.md#plan-mode)) |
| Delegation | `agent`, `agent_swarm` (see [Sub-agents](agents.md)) |
| Background work | `task_list`, `task_output`, `task_stop`, `wait_for` |
| Scheduling | `cron_create`, `cron_list`, `cron_delete` |
| Goals | `create_goal`, `get_goal` |
| Interaction | `ask_user`, `ask_user_question` |
| Media | `read_media` |
| Tool disclosure | `select_tools` |

`update_goal` and `set_goal_budget` are also registered but stay hidden until a
goal is active. `web_reader` appears only when the provider has a page reader,
and `skill` only when a skill registry is loaded. On Kimi, the provider's own
`$web_search` built-in is offered too, backed by the same search service.

A tool that cannot work is not offered: the model never sees it, and `/tools`
does not list it. If the model asks `select_tools` for one, it is told why.

- **`web_search`** needs a search service: the provider's own, or `search_url`.
  On a GLM Coding Plan, it is offered only with `search_url`; the plan's hosted
  search server serves the model instead.
- **`read_media`** needs a model that takes images or video, or a vision fallback.
- **`skill`** needs a skill the model may run.

This list is pinned by `crates/agent-core/tests/builtin_tools_snapshot.rs`:
adding or renaming a tool fails that test until this table is updated.

## Files

- **`read_file`** reads text files in bounded, resumable pieces; see
  [Output limits](#output-limits). Each line comes back numbered:
  `<number><TAB><text>`, the number 1-based, so the model cites lines and pages
  by them exactly. The numbers are not part of the file.
- **`write_file`** creates or replaces a file, and refuses to overwrite one the
  agent has not read or that changed since; see
  [Write safety](permissions.md#write-safety).
- **`edit_file`** replaces text as `read_file` shows it: exactly one match unless
  `replace_all` asks for every one, with line endings and copied line numbers
  handled as described in [Write safety](permissions.md#write-safety).
- **`glob`** lists files only, most recently modified first, 100 per page
  (`limit`, `0` for no count limit), and names the `offset` that continues a page
  cut short by the count or by the output budget. It honours `.gitignore` and
  `.ignore` unless `include_ignored` is set, lists hidden files, and always skips
  VCS metadata such as `.git`. `path` scopes the search to a directory. Patterns
  follow `.gitignore` rules: `*.rs` matches at any depth, `src/*.rs` only
  directly inside `src`, and `src/**/*.rs` at any depth below it. Matches inside
  the working directory are listed relative to it, with `/` between folders on
  every platform.
- **`grep`** and **`list_dir`** search and list within the workspace.

The file tools resolve paths within the working directory and the allowed paths
(`--allow-path`, `allowed_paths`, `--add-dir`). `read_file` and `grep` can also
open this session's spill files and its records, which lie outside them.

When a tool touches a subdirectory that has its own `AGENTS.md`, `CLAUDE.md` or
`.rusta/AGENTS.md`, and the system prompt does not carry it, the next request
names it in a reminder (see [Architecture](architecture.md#the-system-prompt)).

## Shell and background work

`bash` runs a command with `sh -c` (Git Bash on Windows) in the working
directory, or in its `cwd`. A command's stdin is the call's `stdin`, or empty.

A command is stopped with what it started. Each command runs in a process group
of its own (on Windows, a process tree): `task_stop` and a background task's
timeout send the group SIGTERM and, two seconds later, SIGKILL to what is left,
and a foreground command killed at its timeout, or whose call is cancelled, is
killed with everything it started. A command that finishes leaves alone what it
started on purpose, such as `server > log 2>&1 &`. A task is marked finished
only once its output is all in.

A foreground command that runs past its timeout is moved to the background, not
killed: it keeps running under a fresh, longer deadline and the call returns a
task id, so a slow build or a migration is never cut off mid-way. Set
`auto_background_on_timeout: false` on the call to have it killed instead.

Background tasks — commands run with `run_in_background`, and sub-agents — are
listed with `task_list`, read with `task_output` and stopped with `task_stop`.
Their completion is reported to the agent on its own. `wait_for` blocks inside
the turn until a background task finishes, for when the next step needs that
result:

- **`timeout`** is required, from 1 to 90 seconds; pick it from how long the task
  is expected to take, not the maximum. Running out of time is not an error: the
  result lists the tasks still running.
- **`task_id`** picks the task to wait for. Without it, the wait ends when any
  task that was running at the call finishes.
- **The result** includes each finished task's output. That task is then not
  announced again at the next turn.
- **Scope.** Waiting never stops a task, and interrupting it ends only the wait.
  Each agent waits only on tasks it started: a sub-agent's tasks report to the
  sub-agent, and its parent's tasks report to the parent.
- **Sub-agents and repeated waits.** A sub-agent's turn ending is its hand-off,
  and a task that finishes after that reports to no one, so a sub-agent's
  timed-out wait tells it to wait again rather than end its turn. From the second
  `wait_for` call in a turn, every result ends with a `[wait_warning]`: the call
  count, the total time already waited, and advice to do other work instead — a
  sub-agent, an active goal, and the main agent each get their own wording.

`background_max_running_tasks` (`RUSTA_BACKGROUND_MAX_RUNNING_TASKS`) caps how
many background tasks run at once.

## Web tools

**`fetch`** reads a URL itself. A web page comes back as its readable content in
Markdown, extracted with Mozilla's Readability: the title, headings, text, lists,
tables, code and links, without navigation, scripts or styles. Other text (JSON,
plain text, Markdown) comes back as it is, and binary content is not shown. The
Wikipedia article on the Luhn algorithm comes back in 13 thousand characters
instead of its 121 thousand of HTML. Because the model chooses the URLs, `fetch`
refuses loopback, private, link-local and cloud-metadata addresses — checked
after DNS resolution and again on every redirect, with each connection pinned
to the approved address.

**`web_search`** and **`web_reader`** use the provider's own services when it has
them:

- **Kimi Coding Plan:** its `/search` for `web_search` and `$web_search`, and its
  `/fetch` for `web_reader`, which returns the page as Markdown. If that fetch
  fails, Rusta fetches the page itself, as `fetch` does.
- **GLM, pay-per-token:** Z.AI's `/web_search`, with its platform's engine
  (`search-prime`, or `search_pro` on `open.bigmodel.cn`), and its `/reader`.
  They are paid per use, and a Coding Plan does not cover them: on a Coding Plan
  the hosted `zai_web_search` and `zai_web_reader` MCP servers search and read
  instead (see [Z.AI MCP servers](providers.md#zai-mcp-servers)).
- **DeepSeek:** its Search API (`/v1/tools/search`) for `web_search` and its
  Fetch API (`/v1/tools/fetch`) for `web_reader`, with the session's DeepSeek
  key, on the host of its base URL. Both are paid per successful request. A
  search sends along the prompt the turn answers, on one line and cut to 128
  characters, as the API's `ori_question`, which DeepSeek reads the query's
  intent from; `/web` sends none. Each result shows its summary, the text
  DeepSeek writes for a model, on one line and cut at 800 characters. A fetched
  page comes back as Markdown, without the backslash DeepSeek puts before each
  punctuation mark, so its headings and lists read as such. If the fetch fails,
  Rusta fetches the page itself.

A configured `search_url` (`RUSTA_SEARCH_URL`, with `{query}` as the
placeholder) takes precedence over Kimi's and DeepSeek's search. For other
providers it is the only search backend.

The model follows Rusta's `<system-reminder>` notes as the harness's own, so web
content cannot carry one: in what `web_search`, `web_reader` and `fetch` return,
the `<` of any tag that opens or closes a system reminder is escaped as `&lt;`.
The system prompt also tells the model that web pages and search results,
whichever tool brings them in, are untrusted data, never instructions.

## Media

`read_media` shows an image or a video to the model, in the form its provider
takes:

- **Images** are sent at most 256 KiB each (`image_read_byte_budget`,
  `RUSTA_IMAGE_READ_BYTE_BUDGET`) and 2000 px on the longest edge
  (`RUSTA_IMAGE_MAX_EDGE_PX`); larger images are downsampled and re-encoded, with
  EXIF orientation applied. `full_resolution` keeps an image as it is, within
  the provider's own edge limit: DeepSeek takes at most 4096 px per side, so a
  larger image is scaled to that.
- **Formats.** Each image arrives in a format the provider takes: Kimi accepts
  PNG, JPEG, GIF, WebP, BMP, HEIC and HEIF; Z.AI accepts PNG, JPEG and WebP, so
  GIF and BMP are converted; other endpoints get PNG, JPEG, GIF or WebP. HEIC is
  sent only where it is accepted, and elsewhere the result gives a conversion
  command.
- **A model that reads no images** gets a description from the vision fallback,
  which sees the image as the model would, `full_resolution` included.
  `question` asks it something specific ("transcribe every label") instead of a
  general description.
- **Video** goes to models that take it: on the Kimi Code endpoints it is
  uploaded through Kimi's Files API, on GLM-5.3-Flash it is sent inline, and a
  generic endpoint needs `inline_video = true` (`RUSTA_INLINE_VIDEO=1`).
  Otherwise the result says why the video cannot be shown. No provider takes
  audio.

History outlives a model switch, so every request re-checks its media against
the provider it is about to reach, and replaces what cannot go with a note.
**One request** carries at most 20 MB of inline media; past that, the oldest
items are left out of the request, down to 10 MB. If the provider still rejects
a request as too large, such as Kimi's `total message size … exceeds limit
2097152`, the oldest media is replaced in the history with a note naming its
file, and the request is retried. **Kimi uploads** happen once per file and are
referenced as `ms://<id>` afterwards; the ids are remembered under
`<data dir>/rusta/media-cache/` for seven days, and a file Kimi no longer has is
uploaded again.

## Scheduled prompts

`cron_create` schedules a **prompt**, not a shell command: the text is wrapped in
a `<cron-fire>` envelope and re-injected into *this* session when it is idle. A
5-field cron expression in local time (`M H DoM Mon DoW`) says when; `recurring`
(default `true`) says whether it repeats. The schedule is polled, not
interrupt-driven: the TUI ticks once a second while no turn runs and starts a
turn on the first envelope, and the headless keepalive polls the same way. The
REPL polls when you enter a line: it announces what came due and delivers the
envelope ahead of your next message. A fire never lands mid-turn.

- **Coalescing.** A session that was asleep gets one delivery per task, with
  `coalescedCount` saying how many fires it replaces. So does an idle REPL: a
  task that keeps coming due before your next message is announced once and
  delivered once, its runs counted.
- **Expiry.** A recurring task older than 7 days fires one final time with
  `stale="true"` and is then deleted; a one-shot (`recurring: false`) deletes
  itself after its single fire.
- **Jitter.** Fires are shifted deterministically per task — recurring ones by
  up to 10% of their period (capped at 15 minutes) forward, one-shots pinned to a
  `:00`/`:30` mark by up to 90 seconds earlier — so many sessions do not hit the
  API on the same minute.
- **Limits.** 50 tasks per session, an 8 KiB prompt, a 5-year horizon on the
  expression, and one-shots must fire within 350 days. `cron_list` shows every
  task sorted by next fire time; `cron_delete` cancels by id.
- **Gating.** `cron_create` and `cron_delete` need approval under the
  `schedule_prompt` rule, and both refuse in plan mode and on sub-agents.
  `cron_list` is read-only.
- **Persistence.** Tasks live per session in `<records>.cron.json` and come back
  on resume. `/new` empties the schedule; a session forked with `rusta fork`
  starts with no tasks (the source session keeps running its own) and is told so,
  and that it may schedule its own.
- **Turning it off:** `RUSTA_DISABLE_CRON` (or its alias `KIMI_DISABLE_CRON`),
  any truthy value.

## Todo list, goals and skills

- **`todo_list`** keeps the agent's plan of work. While it has items open, the
  session's agent is reminded of it, with the list, after five steps without a
  `todo_list` change, and again every five steps until it changes.
- **Goals** (`create_goal`, `get_goal`, and `update_goal` / `set_goal_budget`
  once one is active) let a long task carry on across turns until it is
  completed, paused, blocked or out of its token budget. `/goal` manages them
  from the REPL and the TUI, and a headless run keeps going while a goal is
  active.
- **`skill`** lists each skill the model may run. Each entry gives the skill's
  `description` (or, without one, its first line, cut to 250 characters) and its
  `when_to_use`. A skill whose frontmatter sets `disable_model_invocation: true`
  is kept for the user: it is left out of that list, the model is refused if it
  names it, and the user runs it with `/skill <name>`. Skills are read from
  `~/.config/rusta/skills`, `~/.agents/skills`, the project's `.rusta/skills`,
  and the directories in `skill_dirs`.

## Output limits

A tool result stays in the conversation until compaction, so no single result
may flood the context:

- **Every tool result** is capped at 50,000 characters (`tool_output_max_chars`,
  `RUSTA_TOOL_OUTPUT_MAX_CHARS`). Longer output keeps its head and tail. The full
  text is saved to a spill file under `<data dir>/rusta/spill/<session>/`, up to
  10 MB, and the result says where. `read_file` and `grep` can read this run's
  spill files even though they lie outside the allowed paths. Spill directories
  untouched for seven days are removed at startup.
- **`read_file`** bounds itself instead. A call returns at most `max_chars`
  characters (default 100,000; at most `read_max_chars`, 500,000 unless changed)
  and prefers whole lines; the line numbers count toward `max_chars`. When the
  budget cuts a read short, the result ends with the exact `offset` and, for a
  line too long to fit, `column_offset` to continue from. A negative `offset`
  reads from the end (`-100` = the last 100 lines). UTF-16 files are converted
  for display. Bytes that are not valid UTF-8 are shown as U+FFFD with a warning
  rather than failing the read. Binary files are refused with a pointer to
  `read_media` or `bash`.
