# Sub-agents

The session's agent can delegate work to sub-agents with `agent`, or run many at
once with `agent_swarm`. Each sub-agent runs in its own context, with a profile
that decides its role and tools, and hands back its last message.

- [When the agent delegates](#when-the-agent-delegates)
- [Profiles](#profiles)
- [The `agent` tool](#the-agent-tool)
- [Forking](#forking)
- [Agent swarm](#agent-swarm)
- [Sub-agent model pool](#sub-agent-model-pool)
- [Stopped turns](#stopped-turns)

## When the agent delegates

The agent launches a sub-agent only when asked to: by you (a sub-agent,
delegation, parallel agents, or a type by name), by `AGENTS.md` or a skill, or by
swarm mode you turned on. A large task, or a request for depth or research, is
not asking, and neither are the types' descriptions, which only choose a type
once launching is asked for. Resuming a sub-agent to go on with work already
asked for needs no new request.

Delegation nests one level by default: the session's agent delegates, and its
sub-agents do the work themselves. `max_subagent_depth`
(`RUSTA_MAX_SUBAGENT_DEPTH`) raises the bound, or `0` forbids delegation. A
sub-agent runs at most `subagent_timeout_ms` (two hours by default) before it is
cancelled.

## Profiles

Four profiles are built in:

- **`explore`** — read-only codebase exploration: `bash` (read-only commands by
  prompt), `read_file`, `read_media`, `glob`, `grep`, `web_search`, `fetch`, and
  the session's read-only MCP tools. It cannot write, edit, delegate or schedule.
  Its first message opens with the repository's git context — branch, dirty
  files, recent commits — when the working directory is a work tree, and nothing
  when it is not.
- **`plan`** — read-only implementation planning, with no shell: `read_file`,
  `read_media`, `glob`, `grep`, `web_search`, `fetch`, and the session's
  read-only MCP tools. Its deliverable is the plan, returned as its final
  message.
- **`coder`** — everything except `agent`, `agent_swarm`, `ask_user`,
  `ask_user_question`, `exit_plan_mode` and the goal tools: it edits, runs
  commands and verifies, but cannot spawn agents, question you or set goals.
- **`default`** — everything except the main-agent-only tools (`agent_swarm`,
  `ask_user`, `ask_user_question`, `exit_plan_mode`, the goal tools). `agent`
  itself stays on the table: nesting is gated by `max_subagent_depth`, not by the
  profile.

The provider's own built-ins go to every sub-agent — on Kimi that is
`$web_search` — and are never cut by a profile's list: the provider executes
them server-side.

**Custom profiles** are YAML files in `~/.config/rusta/profiles/` or the
project's `.rusta/profiles/`:

```yaml
name: reviewer
extends: explore
description: Reviews a change for correctness and style.
when_to_use: When the user asks for a review of a diff or a branch.
system_prompt: You review code changes and report problems with file:line citations.
denied_tools: [web_search, fetch]
mcp_tools: read_only
```

A profile narrows the tool table by listing what stays (`allowed_tools`) or what
goes (`denied_tools`); both combine, and `extends` inherits them additively.
`git_context: true` prepends the repository's git context to the first message.
`mcp_tools: true` gives the profile the session's MCP tools, deferred as they are
for the session and loaded with `select_tools`, with the Devscriptor guidance;
`mcp_tools: read_only` gives only the tools whose server declares them read-only
(MCP's `readOnlyHint`; Devscriptor declares 59 of its 75), or whose server entry
sets `read_only = true`, with guidance that leaves out scanning, editing and
recording notes. `coder` and `default` take them all; `explore` and `plan` take
the read-only ones, so the agents that research code can search and navigate it
with Devscriptor. `--profile` (`RUSTA_PROFILE`) runs the session's own agent
under a profile.

A sub-agent's system prompt is the session's — the same working rules,
environment and instruction files — with its profile's role in place of the
session's, framed as a sub-agent's (its caller sees only its last message). It
leaves out plan mode, which only the user turns on for the session, and carries
the Devscriptor guidance in the form for its own tools.

**Approvals.** A sub-agent is held to the session's permission mode and rules,
and what needs approval is asked through the session. No built-in profile
questions you itself: `ask_user` and `ask_user_question` stay with the session's
agent, and a custom profile that keeps them asks through the session too. The
prompt names the sub-agent, and questions come one at a time. A sub-agent running
in the background cannot ask: what would need an answer is denied.

## The `agent` tool

The `agent` tool lists every type for the model choosing one: its description,
when to use it, and its tools — the ones it lists or every tool but the ones it
denies, and which of the session's MCP tools it gets. A call names the
`profile`, the `prompt`, and optionally `run_in_background`.

Each delegation gets an `agent_id` and keeps its records, with a note of its
profile and model, so a resumed sub-agent comes back as the type and on the
model it started with. Call `agent` with that `agent_id` and `resume: true` to
continue it.

## Forking

`agent` takes a `fork` flag. With `fork: true` the sub-agent starts from a
snapshot of the calling agent's conversation instead of an empty context,
inheriting its instructions, tools and model — for work that needs what has
already been established and would be lossy to re-explain. The tools are the
caller's own table, in the caller's order, so the fork's first request repeats
its caller's request prefix and hits the prompt cache. The snapshot is
**one-time**: the two conversations do not stay in sync.

The inherited turns are followed by a notice saying they came from another
agent, without which the model would continue that conversation instead of doing
its task. The `agent` call that created the fork is still open in the snapshot
and is answered with an explicit "still executing when you were spawned; outcome
unknown", rather than left dangling, which providers reject. `fork` refuses
`resume`, `profile`, and a `model` other than `primary`. A fork keeps its
records too, with its caller's instructions, under which it resumes; it starts
without the caller's scheduled prompts, which stay with the session.

## Agent swarm

`agent_swarm` launches one sub-agent per entry of `items`, each with
`prompt_template`'s `{{item}}` filled in, and resumes the sub-agents named in
`resume_agent_ids` (an `agent_id` → prompt map) before them. The type is
`subagent_type`, `coder` unless named; resumed members keep their own.
`description` names the swarm.

- **Only in swarm mode,** which only you turn on. Outside it, a call starts
  nothing: the model hears that swarm mode is off, and is told to point you to
  `/swarm` only if you asked for a swarm. `agent_swarm` stays in the tool list
  either way, so the prompt cache holds.
- **Checks, before anything starts:** at least two items unless something is
  resumed; a `prompt_template` with `{{item}}` whenever there are items; distinct
  prompts; at most 128 members.
- **Alone.** In swarm mode, `agent_swarm` must be the only tool call in its step.
  A step that pairs it with other calls, or makes two, is refused whole. Outside
  it, only the swarm is refused, and the other calls run.
- **Pacing.** The first five members start at once, then one every 700 ms, up to
  `swarm_max_concurrency` at a time (all, when unset), each within
  `swarm_timeout_ms`.
- **The report** is an `<agent_swarm_result>`: a summary of the outcomes, and one
  `<subagent>` per member with its `agent_id`, `item`, `outcome` and any
  `stop_reason`. When something is unfinished, it says to resume those members
  through `resume_agent_ids`.
- **`fork: true`** starts every item-spawned member from a snapshot of the
  conversation, as `agent`'s `fork` does. It cannot be combined with
  `resume_agent_ids`, another `subagent_type`, or a `model` other than `primary`.
  Forked members get an `agent_id` and keep their records like the others.

`/swarm` turns on swarm mode, in which the agent splits the work it is given
across `agent_swarm`, until `/swarm off`; `/swarm <task>` runs one task in it. A
`-p` run swarms only when it starts in swarm mode, with `--swarm` (or
`RUSTA_SWARM`). The conversation hears when swarm mode starts and when it ends.

## Sub-agent model pool

Every sub-agent inherits the parent's model by default, which is the wrong
default on cost: a throwaway "find where this symbol is defined" delegation runs
on the flagship at `max` effort when a small fast model would answer it as well.
Configure a pool and `agent`/`agent_swarm` gain a `model` parameter:

```toml
[subagent]
default_model = "fast"

[subagent.models.fast]
model = "glm-5.3-flash"
description = "Locating files, reading configs, simple lookups."
reasoning_effort = "low"

[subagent.models.deep]
model = "glm-5.3"
description = "Architecture, tricky bugs, anything needing real reasoning."
```

- **What `agent` lists.** Each alias, with what it is for, the default first.
  Then `primary (= <your model>)`: the calling model at its own reasoning effort.
  `primary` is reserved and always available. Pool entries run at their own
  reasoning effort.
- **What `agent_swarm` lists.** The same choices, on one line. They bind the
  item-spawned members; resumed members keep their own model.
- **Unknown aliases fail the call.** Falling back would bill the flagship for
  work deliberately routed elsewhere, and say nothing.
- **Validation.** The pool is checked at startup, so a typo refuses to start
  rather than surprising you mid-delegation.

With no `[subagent]` section the parameter is not advertised at all.

## Stopped turns

A turn ends when the model stops calling tools. Two things stop it earlier:

- **The repeat breaker.** A tool call repeated across steps earns a reminder: at
  3, 5 and 8 in a row, and on repeats interleaved with other calls. Twelve in a
  row stop the turn. `repeat_breaker = false` (or `RUSTA_REPEAT_BREAKER=0`) turns
  off both the reminders and the stop. A duplicate call within one step still
  runs only once.
- **The step cap.** `max_turns` limits a turn to that many model round-trips
  (default 100).

Either way, the model gets one more step, with tools refused, to hand the work
back: what it completed, what is blocked, and the next step. The result carries a
`stop_reason`, `repeat_breaker` or `max_turns`. Front-ends show it as a
`[stopped]` line, or as a `stop` event under `--stream-json`. A headless run that
hit the step cap still exits with status 1.

A turn **you** cancel — `Ctrl+C` in the REPL; `Ctrl+C` or `Esc` in the TUI — is
followed by a reminder for the model: the partial output above is incomplete, and
your next message continues the conversation. The reminder is written with the
interrupted turn, so `/undo` removes it together with it, and a resumed session
still sees it. SIGTERM, timeouts and the step cap get no reminder.

Sub-agents report the same way. A sub-agent can also stop because it was
cancelled, it failed, or it ran past `subagent_timeout_ms` (`swarm_timeout_ms`
for swarm members), which reports `timed_out`. In every such case its result
ends with `stop_reason`, its `agent_id`, and how to resume it.
