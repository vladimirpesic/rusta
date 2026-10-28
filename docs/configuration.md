# Configuration

Rusta reads its settings from command-line flags, environment variables and
TOML config files. `rusta --help` lists every flag with the variable that sets
it.

- [Config files](#config-files)
- [Where Rusta keeps its files](#where-rusta-keeps-its-files)
- [A complete example](#a-complete-example)
- [Environment variables](#environment-variables)
- [Network](#network)

## Config files

Rusta **layers** its config files, lowest precedence first:

1. `~/.config/rusta/config.toml` — user-wide, where `rusta init` stores your key
   (see [Where Rusta keeps its files](#where-rusta-keeps-its-files) for macOS and
   Windows);
2. `.rusta.toml` in the session's working directory — project settings;
3. a file named with `--config` or `RUSTA_CONFIG` — an explicit choice for one
   invocation.

Each layer overrides the one below key by key, so a project can keep its own
permission rules and MCP servers while the key stays in the user config. Lists
(`permissions.rules`, `mcp_servers`, `allowed_paths`, …) concatenate with the
higher layer first; a duplicate MCP server name resolves to the higher layer.
CLI arguments override environment variables, which override the merged file
values.

Project settings that reach beyond the project — MCP servers, endpoints and
keys, looser permissions, paths outside the folder — apply only once you trust
the folder; see [Workspace trust](permissions.md#workspace-trust). A project
file that declares `[[hooks]]` is refused outright; see [Hooks](hooks.md).

`model`, `base_url` and `api_key` belong to the config's `provider`. Naming a
different one — `rusta --provider glm` on a Kimi-configured machine, or
`/provider glm` in a session — does not inherit them: the endpoint and model are
derived again, and the key is taken from that provider's own environment
variable. Otherwise a Kimi key would be sent to Z.AI, whose rejection reads like
a bad key.

`rusta init` edits the user config rather than replacing it, so permission
rules, MCP servers and sampling settings you have set stay put. A `model` pinned
for the provider you are leaving is dropped, and it says so.

## Where Rusta keeps its files

The paths in these docs are Linux's. Rusta keeps three folders of its own, each
a `rusta` folder inside the platform's:

| Folder | Holds | Linux | macOS | Windows |
| --- | --- | --- | --- | --- |
| Config | `config.toml`, instruction files, profiles, skills, MCP credentials | `~/.config` | `~/Library/Application Support` | `AppData\Roaming` |
| Data | sessions, history, trusted folders, background tasks, spilled tool output, cached media | `~/.local/share` | `~/Library/Application Support` | `AppData\Roaming` |
| Cache | model catalogs | `~/.cache` | `~/Library/Caches` | `AppData\Local` |

`XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME`, set to an absolute
path, take the place of the three on every platform: with
`XDG_CONFIG_HOME=/srv/me/config`, the user config is
`/srv/me/config/rusta/config.toml` on macOS and Windows too.

## A complete example

Every key is optional.

```toml
provider = "glm"
api_key = "zai-..."
# Or name the environment variable holding the key instead (not both). It must
# be set when Rusta starts; the provider's own variable is then not consulted.
# api_key_env = "WORK_ZAI_KEY"
# Which quota the key is billed against: subscription (default) | api.
# This picks the endpoint, and with it the default model.
billing = "subscription"
# global (default) | china
region = "global"
# Both are optional: leaving them out lets provider + billing choose the
# flagship and its endpoint. Set them to pin a specific one.
# model = "glm-5.3"
# base_url = "https://api.z.ai/api/coding/paas/v4"
records = "~/.local/share/rusta/records.jsonl"
permission_mode = "accept_edits"
plan = false

# Extra HTTP headers on every model request (a gateway's routing key, a tenant
# id), below the provider's own; Authorization cannot be set here. They also go
# with Kimi's model list and file uploads. RUSTA_CUSTOM_HEADERS adds more.
# [custom_headers]
# X-Gateway-Route = "coding"

# Headless (-p) runs: what happens to background work once the prompt is
# answered. steer (default) holds the turn for sub-agents and keeps going while
# goals, cron jobs or their notifications are pending; drain waits for
# sub-agents without another model turn; exit ends at once.
# headless_background_mode = "steer"
# Limits for waiting (drain) and steering (steer): a wall-clock ceiling in
# seconds (unset: none) and a cap on follow-up turns (default 100000).
# headless_wait_ceiling_s = 3600
# headless_max_turns = 20

# Sampling / budget
# Cap on a whole model request, response included. Unset means uncapped, which
# is usually right for streaming. 0 also disables it.
timeout = 600
# How long a request may go silent before it is abandoned and retried. Resets
# on every chunk, so this catches a stalled connection without cutting off a
# long, healthy answer. Defaults to 300; 0 disables it.
idle_timeout = 300
# Max model round-trips in one turn before it stops with a handoff.
max_turns = 100
# Attempts for a single model call before its error is surfaced. Counts the
# first attempt, so 1 disables retrying. Defaults to 10.
max_attempts_per_step = 10
# Retry failing model calls without limit instead — for unattended runs that
# should wait out an outage or a spent quota window rather than fail.
# infinite_retry = false
# How deep sub-agent delegation may nest. 1 (the default) lets the session's
# agent delegate while its sub-agents do the work themselves; 2 lets them
# delegate once more. 0 forbids delegation.
max_subagent_depth = 1
# How long a delegated sub-agent may run before it is cancelled, in ms, whether
# it runs in the background or not. 0 removes the deadline. Defaults to two hours.
subagent_timeout_ms = 7200000
# The same for each agent_swarm member, and how many run at once (unset: all).
swarm_timeout_ms = 7200000
# swarm_max_concurrency = 3
# Ask before dangerous shell commands (rm -rf, shutdown, dd to a device, ...) in
# the default and accept_edits modes; bypass_permissions (--yes) never asks.
# false also turns off the built-in prompts before writing secrets and before
# destructive git commands.
dangerous_command_guard = true
# Reminders for a tool call repeated across steps, and a stop with a handoff after
# a long run of one call. false turns both off; a duplicate within one step still
# runs once.
repeat_breaker = true
# Title an untitled session after a completed turn in the REPL or the TUI, from its
# first exchange. false leaves sessions untitled until /title.
auto_session_title = true
# Wire field this endpoint uses for reasoning: reasoning_content (default),
# reasoning, or reasoning_details. Unset, Rusta detects it per endpoint.
# reasoning_key = "reasoning"
max_tokens = 8192
# Sampling. Kimi fixes both for every model it serves, so it is sent neither.
temperature = 0.6
top_p = 0.95
thinking = true
# Preserved Thinking (see docs/providers.md). Kimi enables it by default when
# `thinking` is on. GLM defers to the endpoint.
thinking_keep = false
# How long Kimi's platform keeps a written prompt-cache entry, for its K3 series:
# "5m" or "1h". Unset: "1h" in an interactive session, "5m" in a headless run.
# prompt_cache_ttl = "1h"
# none | minimal | low | medium | high | xhigh | max, mapped onto the model's
# own effort ladder.
reasoning_effort = "high"

# Filesystem and context
allowed_paths = ["/srv/data"]
additional_dirs = ["../sibling-repo"]
project_context_files = ["NOTES.md"]
project_context_budget_tokens = 8192
skill_dirs = ["~/.config/rusta/skills"]

# Concurrency
background_max_running_tasks = 4

# Raw-byte budget for one image read_media sends (0 = none).
image_read_byte_budget = 262144
# Send video inline to a generic OpenAI-compatible endpoint's vision model.
# inline_video = false

# Model requests one context compaction may make (retries and shrinks count)
# before it falls back to evicting the oldest messages.
compaction_max_attempts = 5

# Tool output (see docs/tools.md)
# Budget for one tool result, in characters. Larger output is cut to a head and
# a tail, and the full text is saved to a spill file. 0 disables the cap.
tool_output_max_chars = 50000
# Ceiling on the max_chars one read_file call may ask for. 0 removes it.
read_max_chars = 500000

# Built-in web_search tool
search_url = "https://example.com/search?q={query}"

# Global MCP timeouts. Applied to every server that does not set its own, and
# over Rusta's own for the auto-detected ones (Devscriptor 60 s / 180 s, the
# npx vision server 120 s / 120 s), which have no `[[mcp_servers]]` entry to
# edit and so cannot be tuned any other way.
# Precedence: per-server > RUSTA_MCP_* > here > built-in (30 s / 60 s).
[mcp]
startup_timeout_ms = 30000
tool_timeout_ms = 60000
# Load every server's tools on demand unless the server opts out with its own
# `deferred = false`. Unset: a server defers only when it has more than 10
# tools. Precedence: per-server > RUSTA_MCP_DEFAULT_DEFERRED > here > size rule.
# default_deferred = true

# Models sub-agents may be bound to. Without this section every sub-agent
# inherits the parent's model, and the delegation tools advertise no `model`
# parameter at all. `primary` always means "the calling model" and is reserved.
[subagent]
default_model = "fast"

[subagent.models.fast]
model = "glm-5.3-flash"
description = "Locating files, reading configs, simple lookups."
reasoning_effort = "low"

[subagent.models.deep]
model = "glm-5.3"
description = "Architecture, tricky bugs, anything needing real reasoning."

# Extra MCP servers. Devscriptor is auto-detected and needs no entry here.
[[mcp_servers]]
name = "http-tools"
url = "http://localhost:8000/mcp"
# Tools always in the tool table (false) or loaded on demand with
# select_tools (true). Unset: the [mcp] default_deferred applies, and when
# that is unset too, deferred only when the server has more than 10.
# deferred = false
# Tools that only read, though the server does not declare it (MCP's
# readOnlyHint): taken as declared read-only, so the explore and plan
# sub-agents get them and they run without approval. A tool that declares
# otherwise is taken at its word.
# read_only = true

[[mcp_servers]]
name = "hosted-tools"
url = "https://mcp.example.com/mcp"
# OAuth with PKCE. auth_url and token_url may be left out: they are discovered
# from the server (RFC 9728, then RFC 8414 / OpenID discovery).
[mcp_servers.oauth]
client_id = "my-client"
scopes = "tools:read"

[[mcp_servers]]
name = "backend-tools"
# On Windows a bare name is resolved through PATH x PATHEXT, so `npx` finds
# `npx.cmd` — most published MCP servers are launched exactly that way.
command = "my-mcp-server"
# Working directory for a spawned server. Relative paths resolve against the
# session's `--cwd`. Omit to inherit Rusta's own directory.
cwd = "backend"

# Permission policy; first matching rule wins (see docs/permissions.md).
[[permissions.rules]]
name = "allow-reads"
tool = "read_file"
action = "allow"
```

This example is checked by the test suite: it must deserialize into Rusta's
config type.

## Environment variables

### Core

| Variable | Description |
| --- | --- |
| `RUSTA_CONFIG` | Path to a config file |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` | Where Rusta's config, data and cache folders go, on every platform; see [Where Rusta keeps its files](#where-rusta-keeps-its-files) |
| `RUSTA_PROVIDER` | `kimi`, `glm`, `zai`, `deepseek`, or `openai` |
| `RUSTA_BILLING` | `subscription` (default) or `api`. Selects the endpoint and the default model |
| `RUSTA_REGION` | `global` (default) or `china` |
| `RUSTA_MODEL` | Model identifier. Overrides the default implied by the billing |
| `RUSTA_BASE_URL` | Custom API base URL. Overrides the endpoint implied by the billing and region |
| `RUSTA_API_KEY` | API key |
| `RUST_LOG` | Log filter for diagnostics on stderr, e.g. `warn` or `rusta=debug`. Unset shows errors only |
| `RUSTA_CUSTOM_HEADERS` | Extra HTTP headers for model requests, as newline-separated `Name: value` lines. A header of the same name in the config's `custom_headers` wins; the provider's own headers and `Authorization` are never replaced |
| `RUSTA_HEADLESS_BACKGROUND_MODE` | `steer` (default), `drain` or `exit`: what a headless run does about background work once its prompt is answered |
| `RUSTA_HEADLESS_WAIT_CEILING_S` | Longest time, in seconds, a headless run keeps waiting or steering after its prompt. Unset: no ceiling |
| `RUSTA_HEADLESS_MAX_TURNS` | Most follow-up turns a steering headless run takes for background notifications. Defaults to 100000 |
| `RUSTA_RECORDS` | Path to the JSONL records file |
| `RUSTA_PERMISSION_MODE` | `default`, `accept_edits`, or `bypass_permissions` |
| `RUSTA_PLAN` | Start in plan mode, as `--plan` does |
| `RUSTA_SWARM` | Start in swarm mode, as `--swarm` does: the only way a `-p` run swarms |
| `RUSTA_PROFILE` | Agent profile to load |
| `RUSTA_SESSION` | Session identifier |
| `RUSTA_SESSION_LOCK` | `0` turns off the session lock that keeps a second process from writing a session while one does |
| `RUSTA_RESUME` | Resume a previous session |
| `RUSTA_ALLOW_PATH` | Extra paths tools may access outside the working directory |
| `RUSTA_SEARCH_URL` | URL template for the built-in `web_search` tool |
| `RUSTA_REASONING_KEY` | Wire field for reasoning content: `reasoning_content`, `reasoning`, or `reasoning_details`. Unset, Rusta detects it from the endpoint's own responses |
| `RUSTA_MAX_ATTEMPTS_PER_STEP` | Attempts for a single model call before its error is surfaced. `1` disables retrying; `0` takes the default of 10 |
| `RUSTA_INFINITE_RETRY` | Retry failing model calls without limit (`--infinite-retry`, config `infinite_retry`). Rides out outages, overload and spent quota windows; a context overrun, a malformed request, a content-filter refusal or a local configuration error still ends the call |
| `RUSTA_BACKGROUND_MAX_RUNNING_TASKS` | Maximum concurrently running background tasks |
| `RUSTA_DISABLE_CRON` | Disable scheduled prompts (`cron_create`/`cron_list`/`cron_delete` do nothing). `KIMI_DISABLE_CRON` is honoured as an alias; any truthy value (`1`, `true`, `yes`, `on`) turns cron off |
| `RUSTA_SUBAGENT_TIMEOUT_MS` | How long a delegated sub-agent may run before it is cancelled, in the foreground or the background. `0` removes the deadline; defaults to two hours |
| `RUSTA_SWARM_TIMEOUT_MS` | How long each `agent_swarm` member may run before it is cancelled and reports `timed_out`. `0` removes the deadline; defaults to two hours |
| `RUSTA_SWARM_MAX_CONCURRENCY` | How many `agent_swarm` members run at once (a positive integer). Unset runs them all together |
| `RUSTA_DANGEROUS_COMMAND_GUARD` | `0`/`false` stops `bash` from asking before dangerous commands (`rm -rf`, `shutdown`, `dd` to a device, …) in the `default` and `accept_edits` modes, and turns off the built-in guardrails. Defaults to on; bypass mode (`--yes`) never asks about dangerous commands |
| `RUSTA_REPEAT_BREAKER` | `0`/`false` turns off the repeat breaker: no reminders for a tool call repeated across steps, and no forced stop. Defaults to on |
| `RUSTA_TRUST_WORKSPACE` | `1`/`true` applies this run's `.rusta.toml` settings that need trust without asking or remembering; `0`/`false` leaves them out even in a trusted folder (`--trust-workspace[=false]`). Unset: a trusted folder applies them, and an interactive session asks |
| `RUSTA_MCP_STARTUP_TIMEOUT_MS` | Global MCP startup budget, overriding `[mcp]`. A per-server value still wins |
| `RUSTA_MCP_TOOL_TIMEOUT_MS` | Global MCP per-tool-call budget, on the same terms |
| `RUSTA_MCP_DEFAULT_DEFERRED` | Global default for loading MCP tools on demand (`1`/`true`/`yes`/`on`, or `0`/`false`/`no`/`off`), overriding `[mcp] default_deferred`. A per-server `deferred` still wins |
| `RUSTA_PROJECT_CONTEXT_BUDGET_TOKENS` | Cap on the project instruction files (`AGENTS.md` and the like) in the system prompt, in tokens (default 8192). A file that does not fit is cut, and named in a warning |
| `RUSTA_IMAGE_MAX_EDGE_PX` | Longest-edge ceiling for images sent to a vision model (default 2000). Non-positive or malformed values are ignored |
| `RUSTA_IMAGE_READ_BYTE_BUDGET` | Raw-byte budget for one image `read_media` sends (default 262144; `0` sets none). Larger images are downsampled and re-encoded |
| `RUSTA_INLINE_VIDEO` | Send video inline to a vision model on a generic OpenAI-compatible endpoint (`--provider openai`). Kimi and GLM decide per model |

### Sampling and budget

| Variable | Description |
| --- | --- |
| `RUSTA_TIMEOUT` | Cap on a whole model request, seconds, response included. Unset means uncapped — usually right for streaming, where a long answer is slow because it is working. Stalls are caught by `RUSTA_IDLE_TIMEOUT`, not by this |
| `RUSTA_IDLE_TIMEOUT` | How long a model request may go silent, in seconds, before it is abandoned and retried. Resets on every chunk received. Defaults to 300; `0` disables it |
| `RUSTA_MAX_SUBAGENT_DEPTH` | How deep sub-agent delegation may nest. `0` forbids it. Defaults to 1: sub-agents do not spawn sub-agents of their own |
| `RUSTA_MAX_TURNS` | Maximum model round-trips (steps) in one turn before it stops with a handoff. Defaults to 100. Raise it for long autonomous tasks |
| `RUSTA_MAX_TOKENS` | Maximum tokens to generate |
| `RUSTA_TOOL_OUTPUT_MAX_CHARS` | Budget for one tool result, in characters, before it is cut to a head and a tail and the full text saved to a spill file. Defaults to 50000; `0` disables the cap |
| `RUSTA_READ_MAX_CHARS` | Ceiling on the `max_chars` one `read_file` call may ask for. Defaults to 500000; `0` removes it |
| `RUSTA_COMPACTION_MAX_ATTEMPTS` | Model requests one context compaction may make before it falls back to evicting old messages. Defaults to 5 |
| `RUSTA_TEMPERATURE` | Sampling temperature |
| `RUSTA_TOP_P` | Nucleus-sampling top-p |
| `RUSTA_THINKING` | Enable reasoning/thinking mode |
| `RUSTA_REASONING_EFFORT` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`, mapped onto the model's own ladder: GLM-5.2 and newer (GLM-5.3 folds the seven rungs onto `low`/`high`/`max`), DeepSeek's models, and the Kimi models that list effort levels. On Kimi and DeepSeek `none` turns thinking off, or runs the lowest effort on a model that cannot turn it off. See [Providers](providers.md) |
| `RUSTA_PROMPT_CACHE_TTL` | `5m` or `1h`: how long Kimi's platform keeps a written prompt-cache entry for its K3 series. Unset, an interactive session asks for `1h` and a headless run takes the platform's `5m` |

### Providers

| Variable | Description |
| --- | --- |
| `KIMI_API_KEY` / `ZAI_API_KEY` / `DEEPSEEK_API_KEY` / `OPENAI_API_KEY` | Provider API keys |
| `KIMI_BASE_URL` / `ZAI_BASE_URL` / `DEEPSEEK_BASE_URL` / `OPENAI_BASE_URL` | Provider base URLs |
| `KIMI_MODEL_NAME` / `ZAI_MODEL_NAME` / `DEEPSEEK_MODEL_NAME` / `OPENAI_MODEL_NAME` | Default model per provider |
| `RUSTA_ZAI_API_KEY` | Z.AI key for Z.AI's own services — the hosted MCP servers on a Coding Plan, and the web search and reader on pay-per-token — preferred over `ZAI_API_KEY`; `--api-key` wins over both |
| `ACP_GLM_THINKING` | `1`/`true` or `0`/`false`: switch GLM thinking on or off whatever the settings say. Models that cannot turn it off still think, at the lowest effort |
| `Z_AI_VISION_MCP_URL` | Remote Z.AI vision MCP server, used instead of starting `@z_ai/mcp-server` with `npx` |

### Kimi tuning

These are read by the agent loop regardless of provider, and are the
lowest-precedence source for the value they control.

| Variable | Description |
| --- | --- |
| `KIMI_MODEL_MAX_COMPLETION_TOKENS` | Completion budget. 0 or negative: send no cap at all, not even the provider's default for an uncapped request (an explicit `max_tokens` still applies) |
| `KIMI_MODEL_MAX_TOKENS` | Alias of the above, read when it is unset |
| `KIMI_MODEL_TEMPERATURE` | Temperature, accepted in `0.0..=2.0`. Kimi fixes it for every model it serves, so the Kimi provider does not send it; the other providers do |
| `KIMI_MODEL_TOP_P` | Top-p override. Not sent to Kimi either, for the same reason |
| `KIMI_MODEL_THINKING_EFFORT` | Thinking effort override. Feeds `reasoning_effort` |
| `KIMI_MODEL_THINKING_KEEP` | Preserved Thinking. `on`/`off`, `true`/`false`, `yes`/`no`, `all`/`none`, `1`/`0`. Unset or unrecognized defers to the model default |

### Devscriptor

| Variable | Description |
| --- | --- |
| `DEVSCRIPTOR_BIN` | Explicit path to the `devscriptor` executable; skips discovery |
| `DEVSCRIPTOR_DB` | Index the auto-configured MCP server opens. Defaults to `devscriptor.db` in the working directory — the file `devscriptor scan` writes — resolved to an absolute path so the server cannot fall back to a global index |

## Network

Outbound requests honour `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and
`NO_PROXY`, including SOCKS (`socks5://`, `socks4://`) via `ALL_PROXY`. TLS is
rustls with the bundled Mozilla root certificates, so there is no OpenSSL
dependency.
