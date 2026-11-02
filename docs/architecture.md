# Architecture

Rusta is a Rust workspace of four crates. This page describes how they fit
together, how a turn runs, and the design decisions behind them.

- [Crates](#crates)
- [A turn](#a-turn)
- [The system prompt](#the-system-prompt)
- [Context management](#context-management)
- [Reliability](#reliability)
- [Persistence](#persistence)
- [Design decisions](#design-decisions)

## Crates

```text
crates/
├── lm-bridge/    # Provider abstraction and the Kimi, GLM, DeepSeek and OpenAI-compatible providers
├── agent-core/   # Agent, turn loop, tools, permissions, context, records
├── mcp-client/   # MCP connections, tool adaptation, OAuth
└── rusta-cli/    # The `rusta` binary: CLI, config, front-ends, setup and diagnostics
```

- **`lm-bridge`** normalizes messages, tools and streams across providers. Its
  `generate()` assembler turns a provider's stream into a step, routing parallel
  tool-call deltas to their calls. Each provider owns its wire format: request
  fields, model metadata (from the live `GET /models` catalogue where the vendor
  serves one, cached on disk for a day, with a built-in fallback table), error
  classification, usage extraction and media policy. It also builds every HTTP
  client, with one TLS configuration.
- **`agent-core`** holds the `Agent`: its context memory, the turn loop, the tool
  registry and the 26 built-in tools, the tool scheduler, the permission engine,
  background tasks, sub-agents and profiles, scheduled prompts, goals, user
  hooks, and the JSONL records a session is persisted in.
- **`mcp-client`** connects to MCP servers over stdio or streamable HTTP/SSE,
  keeps them connected, adapts their tools into the agent's tool interface
  (names, schemas, results, argument aliases), and runs OAuth sign-ins.
- **`rusta-cli`** is the binary: argument and config handling, workspace trust,
  the endpoint table, the headless, REPL and TUI front-ends, slash commands,
  `init` and `doctor`, Devscriptor discovery, the providers' native web and
  vision clients, and the session store.

## A turn

A prompt starts a turn, and a turn is a loop of model steps:

1. **The request** is built from the conversation, projected into a form the
   provider accepts (see [Context management](#context-management)), and the tool
   table: the tools that can work in this session, in a fixed order.
2. **The response streams**: answer and reasoning text reach the front-end as
   they arrive, and tool calls are assembled from their deltas.
3. **The step is recorded** — text, reasoning and tool calls together — and
   written to disk before the next request, so a process killed mid-turn keeps
   the steps it took, and Preserved Thinking has the reasoning to replay.
4. **The tool calls run.** Each is validated against its schema, passed through
   the user's hooks and the permission engine, and scheduled: calls whose
   resource accesses do not conflict (two reads of one file, or edits of
   different files) run concurrently, and results keep the order of the calls.
   Identical calls in one step run once. Every result is bounded before it
   enters the context.
5. The loop continues until the model answers without calling a tool, or a
   [stop condition](agents.md#stopped-turns) hands the work back.

Between steps the loop adds what the model should know: steered input from the
TUI, a reminder for a todo list that fell behind, instruction files under
directories the tools touched, announcements of loadable tools, and the
completion of background tasks.

## The system prompt

The session's prompt is
[`system_prompt.md`](../crates/rusta-cli/src/system_prompt.md), filled in at
startup and again on `/reload`. It contains:

- **Working rules:** how to communicate, use tools, write code, handle risky
  actions, deliver and verify work, and what compaction keeps.
- **The environment:** the OS, the shell `bash` uses (`sh -c`, which is Git Bash
  on Windows), and the working directory and its listing. Any `--add-dir`
  directories come with their own listings.
- **The project's instruction files,** within `project_context_budget_tokens`
  (default 8,192). A file that does not fit is cut and named in a warning. They
  are collected, in this order, from the user config directory, the working
  directory and its parents, `--add-dir` directories, and
  `--project-context-file`.
- **Session options:** a `--profile`'s prompt as the session's role, and the
  Devscriptor workflow when its server is configured.

Instruction files deeper in the tree are not loaded up front. When a tool touches
a subdirectory that has its own `AGENTS.md`, `CLAUDE.md` or `.rusta/AGENTS.md`,
and the prompt does not already carry that file, the next request names it in a
reminder. The file tools count, and `bash` through its `cwd`. If the reminder is
compacted away, it comes back until the file has been read. Sub-agents' prompts
carry the session's instruction files, so they too are told only about deeper
ones.

The date is not in the prompt: putting it there would change the prompt daily
and defeat prompt caching. Instead, each agent gets `Today's date is …` in a
reminder at its first prompt, again when the date changes, and again after a
compaction. Reminders arrive as `<system-reminder>` text, which the prompt tells
the model to follow as the harness's own; web content can never carry one (see
[Web tools](tools.md#web-tools)).

## Context management

**Projection.** Before every request the recorded history is projected into a
list the provider accepts. An assistant message that would serialize to nothing
— empty text, or only reasoning, which a turn interrupted mid-thought leaves
behind — is dropped, since Moonshot and Z.AI reject an empty assistant message
on every resend, which would wedge the session. A tool call left without a
result is answered, and duplicate tool-call ids are made unique.

**Compaction.** When the conversation passes 90 % of the model's context window,
Rusta compacts it before the next request instead of waiting for the provider to
reject one. A model that keeps part of its window free for output compacts
before the conversation grows into that part: DeepSeek keeps 256K, so it
compacts at 786,432 tokens of its 1M window.

- **The model writes a handoff summary.** The request carries the real
  conversation, trimmed from the oldest end to fit the window, and asks for the
  goals and constraints in force, decisions made, commands run and files changed
  (with exact identifiers and error lines), what is still unknown, and the
  remaining plan in order. `/compact <instruction>` adds a focus hint.
- **The compacted history** keeps your own messages verbatim: the oldest and the
  most recent, up to about 20,000 tokens. It then holds the summary, with the
  live todo list and a pointer to the replaced lines in the session's records
  file attached below it. Last comes a reminder to continue the work in progress,
  so the model resumes the latest request rather than reading the summary as an
  answer. `read_file` and `grep` can open the records file and its blob
  directory, although they lie outside the allowed paths.
- **If no summary can be had** after `compaction_max_attempts` model requests
  (default 5; retries and shrinks count), the oldest messages are removed
  instead. The first and the latest message you wrote are always kept, and a
  note records how many messages went and where they can still be read.
- **Progress is reported:** `[compaction] …` on stderr in headless mode, a line
  in the REPL and the TUI, and `compaction` events (`started`, then
  `summarized`, `evicted` or `unchanged`) with `--stream-json`.

If the provider still reports a context overrun (including Z.AI's `1261`), the
same compaction runs. Tool results are dropped next, and a final protected
eviction follows before the error surfaces. A resumed session replays recorded
summaries, so it starts from the compacted history, not the full transcript.

## Reliability

- **Retries.** A failing model call is retried up to 10 times
  (`max_attempts_per_step`), with exponential backoff from 0.5 s, doubling to a
  32 s cap, plus up to 25 % jitter; a server's `Retry-After` is honoured.
  `--infinite-retry` retries without limit for unattended runs. A context
  overrun, a malformed request, a content-filter refusal or a local
  configuration error is never retried; a content-filter refusal is
  deterministic, so retrying it would only delay the notice.
- **Broken streams.** A stream that dies mid-response — a reset connection, a
  proxy hanging up, the idle timeout firing (`idle_timeout`, 300 s by default),
  or a body that closes without a finish reason — is retried, and the
  front-end marks the partial output as superseded. So is a generation the
  provider says it cut short (DeepSeek's `insufficient_system_resource` and
  `aborted`, Z.AI's `network_error`): nothing it streamed is kept, and no tool
  call in it runs.
- **Reasoning without an answer.** A reply that stopped short with only
  reasoning is a reply cut short, and is retried. One that finished normally is
  kept, and the model is asked once a turn for the answer it did not write; a
  second such reply ends the turn.
- **Cut-off tool calls** — by the output limit or a content filter — are never
  run: a truncated `write_file` can be valid JSON and still be half a file. The
  model is told to re-issue smaller calls, and three cut-off steps in a row end
  the turn. Malformed arguments fail only their own call, not the whole step.

## Persistence

A session is a JSONL records file (`~/.local/share/rusta/records/<id>.jsonl` by
default) with sidecars beside it: the todo list, goals, active skills, the files
the agent has read, scheduled prompts, and each sub-agent's profile and model.
Large tool results are stored once as blobs and referenced from the records.
`records/index.json` lists the sessions. A session is written by one process at a
time, under a lock file beside its records (see [Sessions](usage.md#sessions)).
Tool output spill files live under `<data dir>/rusta/spill/<session>/`, and Kimi
upload ids under `<data dir>/rusta/media-cache/`; both are pruned after seven
days.

## Design decisions

**Each provider's documented contract, exactly.** A request carries only the
fields its endpoint documents and accepts, and nothing it would silently ignore.
Kimi fixes temperature and top-p, so they are never sent; GLM-5.3 rejects
disabled thinking, so "off" becomes its lowest effort; DeepSeek's low default
output cap would cut long reasoning, so an explicit cap is always sent. Model
capabilities come from the vendors' live catalogues, so a new model needs no
release, with built-in tables for a machine that has never reached the
endpoint.

**Keep the prompt cache warm.** The system prompt and the tool table are the
cached prefix of every request, so they change only when they must: the date
travels in reminders, tools keep a fixed order, tools loaded on demand arrive on
a system message where the model takes them there, `agent_swarm` stays listed
outside swarm mode, `/reload` replaces tools in place, and a forked sub-agent
repeats its caller's exact prefix.

**Progressive tool disclosure.** A large MCP server would crowd the tool table
and every request with definitions the model rarely needs. Its tools are
announced by name and description instead, and loaded with `select_tools` when
needed.

**The user's machine is not the model's to run freely.** Every call passes the
permission engine; secrets and history-rewriting git commands ask even in
bypass mode; dangerous shell commands ask outside it; a cloned repository's
config cannot start servers or redirect requests until its folder is trusted,
and cannot declare hooks at all; `fetch` refuses private and metadata
addresses; and web content cannot pose as the harness.

**Writes from a current picture.** `write_file` overwrites only a file the agent
has read and that has not changed since, per agent; `edit_file` needs its text to
match exactly once. Hooks fail open, so a broken script never blocks work, which
is also why they are not a security boundary.

**Delegation on request.** Sub-agents are launched only when asked for, nest one
level by default, and inherit the session's permissions, so delegation never
widens what is allowed or runs up cost unasked.

**One portable binary.** TLS is rustls with `ring` and bundled root
certificates — no OpenSSL, and no C toolchain hazards — and archives are
written with Deflate alone, so Linux builds can target glibc 2.17 with
`cargo-zigbuild` (see [Building](building.md)).
