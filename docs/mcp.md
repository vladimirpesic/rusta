# MCP servers

Rusta is an MCP client: it connects to servers over child-process stdio or
streamable HTTP/SSE and offers their tools to the model beside its own.
Devscriptor is detected and connected automatically; other servers are listed
in the config.

- [Configuring servers](#configuring-servers)
- [Tool names](#tool-names)
- [On-demand loading](#on-demand-loading)
- [Approvals](#approvals)
- [Results](#results)
- [Authentication](#authentication)
- [Devscriptor](#devscriptor)

## Configuring servers

```toml
[[mcp_servers]]
name = "http-tools"
url = "http://localhost:8000/mcp"

[[mcp_servers]]
name = "backend-tools"
command = "my-mcp-server"
args = ["--stdio"]
cwd = "backend"                      # relative to the session's --cwd
env = { API_TOKEN = "..." }          # only for this server's process
```

| Field | Meaning |
| --- | --- |
| `name` | Unique name; it prefixes the server's tools |
| `command`, `args`, `cwd`, `env` | A server spawned over stdio. On Windows a bare command is resolved through `PATH` × `PATHEXT`, so `npx` finds `npx.cmd`. Without `cwd` the server inherits Rusta's own directory, not the session's |
| `url`, `token` | A server reached over streamable HTTP/SSE, with an optional bearer token |
| `startup_timeout_ms`, `tool_timeout_ms` | This server's budgets, over the `[mcp]` and `RUSTA_MCP_*` defaults (30 s / 60 s built in) |
| `deferred` | Load the tools on demand (`true`) or keep them in the tool table (`false`) |
| `read_only` | Treat every tool as read-only, though the server does not declare it |
| `disabled` | Skip the server |
| `oauth` | Sign in with OAuth (below) |

Project servers in `.rusta.toml` start only in a trusted folder; see
[Workspace trust](permissions.md#workspace-trust). `/mcp` lists the servers and
their state, and `rusta doctor` checks that each one connects.

## Tool names

MCP tools are namespaced `mcp__<server>__<tool>`, in the characters every model
API accepts (`[a-zA-Z0-9_-]`, at most 64): any other character becomes `_`, a
longer name is cut and ends with a short stable hash, and two names that would
come out the same are told apart the same way. Calls still reach the server's
own tool name. A server whose name had to change is named in a warning at
startup. Arguments the model names differently from the tool's schema (`path`
for `file_path`, `top_k` for `limit`, …) are mapped onto the schema's own names.

## On-demand loading

A server's tools sit in the tool table like built-ins unless the server is
large. With more than ten tools they are *deferred*: announced by name and
description, and loaded with `select_tools` when the model needs them, which
keeps the tool table — and with it the prompt cache's prefix — small.

- `deferred = true` or `false` on the server entry overrides the size rule.
- `default_deferred = true` under `[mcp]` (or `RUSTA_MCP_DEFAULT_DEFERRED`) defers
  every server's tools by default; a server can still opt out with its own
  `deferred = false`, which always wins. A global `false` keeps even large
  servers inline.
- **Where loaded tools go.** Kimi models whose model list marks
  `supports_dynamic_tools` — such as `kimi-for-coding`, the `k` ids and the
  platform's K3 series — take tools mid-conversation: a tool loaded with
  `select_tools` arrives on a system message, and the request's tool list stays
  unchanged. Other models get loaded tools in the tool list itself.
- **Announcements.** Changes to the set of loadable tools are announced in
  `<tools_added>` / `<tools_removed>` messages; when nothing is loadable, that is
  said once. A name that keeps failing selection, and a run of calls that load
  nothing, draw a warning to stop guessing.

## Approvals

A tool its server declares read-only (MCP's `readOnlyHint`), or whose server
entry sets `read_only = true`, runs without asking, as `read_file` and `grep` do,
within the same path limits. One the server declares as changing something is
asked about as a write, or as a command when it runs one. For a tool the server
says nothing about, Rusta goes by its name and description: searches and fetches
run without asking, anything else asks. The read-only tools are also the ones the
`explore` and `plan` sub-agents get.

## Results

Results keep everything the server sent:

- text blocks are joined by newlines, and `structuredContent` is added unless a
  text block already carries the same JSON;
- images, such as a browser screenshot, reach vision models the way `read_media`
  sends a file, with the original saved to the spill directory;
- resource links become `[resource: <name> <uri>]`;
- audio and binary resources are saved there and named in the result.

When a server answers a call with "method not found", its tools are listed
again, and the ones it no longer lists are reported removed. A server whose
connection drops is reconnected.

## Authentication

```toml
[[mcp_servers]]
name = "hosted-tools"
url = "https://mcp.example.com/mcp"

[mcp_servers.oauth]
client_id = "my-client"
scopes = "tools:read"
# auth_url, token_url, client_secret and redirect_port are optional.
```

When a server with `[mcp_servers.oauth]` rejects the connection, Rusta runs a
PKCE sign-in in the browser. `auth_url` and `token_url` are discovered when not
configured (RFC 9728, then RFC 8414 or OpenID discovery), and `offline_access`
is requested when the authorization server offers it, so the grant includes a
refresh token. The grant is saved under `<config dir>/rusta/credentials/mcp/`.
It is refreshed two minutes before it expires, and again when the server rejects
it.

When a tool call is refused with 401 mid-session, Rusta first refreshes and
retries the call. If that is not possible, it forgets the rejected token, marks
the server as needing authorization, and restarts the sign-in (or, without
OAuth, offers `mcp__<server>__authenticate`). The model is told to retry once
that is done.

## Devscriptor

[Devscriptor](https://devscriptor.com) is a code-intelligence MCP server: search
and analysis over a persistent index of the codebase, LSP-backed navigation, and
a durable Context Graph. Rusta detects an installed Devscriptor and registers it
as the `devscriptor` server with no configuration. Its guidance for the model
needs **Devscriptor 1.0.0 or newer**, which keeps its index current by itself:
it watches the workspace and re-checks the files behind every answer, and its
LSP results carry a 1-based `cite` (`path:line:col`) for the model to quote.

Discovery order:

1. `$DEVSCRIPTOR_BIN` — an explicit path to the `devscriptor` executable;
2. `$PATH`;
3. the platform install directories:
   - Linux / macOS: `/usr/local/bin`, `/usr/bin`, `/opt/homebrew/bin`,
     `~/.local/bin`, `~/bin`, `~/.devscriptor/bin`;
   - Windows: `devscriptor` and `Programs\devscriptor` under each of
     `%LOCALAPPDATA%`, `%ProgramFiles%` and `%ProgramW6432%`, then
     `%USERPROFILE%\.devscriptor\bin`.

The server is started as `devscriptor mcp` with a 60 s startup timeout and a
180 s per-tool timeout, unless the global `[mcp]` timeouts say otherwise. It is
spawned **in the session's `--cwd`** and handed an explicit absolute
`--database`, so the index it opens and the paths it resolves belong to the
workspace rather than to wherever Rusta was launched from. If the workspace has
no `devscriptor.db`, an existing index further up the same repository is used,
so a session inside a monorepo package reads the repository-wide index instead
of creating an empty one. `$DEVSCRIPTOR_DB` overrides the choice. Devscriptor
has more than ten tools, so they are deferred and loaded with `select_tools`.
An explicit `[[mcp_servers]]` entry named `devscriptor` always wins over
auto-detection.

When Devscriptor is connected, the system prompt carries its workflow: index
first, search and navigate with it, record durable notes in the Context Graph,
and handle its failures by the errors it actually returns. The guidance comes in
a full form and a read-only form, for sub-agents that get only its read-only
tools. [devscriptor-mcp-guide.md](devscriptor-mcp-guide.md) explains how to get
the most out of Devscriptor from any MCP-capable agent.
