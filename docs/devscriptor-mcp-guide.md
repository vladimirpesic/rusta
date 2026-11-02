# Optimizing AI Coding Agents for Devscriptor MCP

A practical, provider-agnostic guide to wiring any AI coding agent or assistant
(Claude Code, Claude Desktop, Cursor, Cline, Continue, Zed, Rusta, or a custom
harness) to the **Devscriptor MCP server** so that all **75 tools** are used
correctly, efficiently, and without hallucination.

It describes Devscriptor 1.0.0 and newer, which keeps its index current by
itself and gives LSP results a quotable `cite`.

---

## Table of contents

- [Optimizing AI Coding Agents for Devscriptor MCP](#optimizing-ai-coding-agents-for-devscriptor-mcp)
  - [Table of contents](#table-of-contents)
  - [1. What you get: the 75 tools](#1-what-you-get-the-75-tools)
  - [2. Step 1 — Use the right binary](#2-step-1--use-the-right-binary)
  - [3. Step 2 — Verify the server before trusting it](#3-step-2--verify-the-server-before-trusting-it)
    - [3a. Tool count](#3a-tool-count)
    - [3b. stdout must be pure JSON-RPC](#3b-stdout-must-be-pure-json-rpc)
  - [4. Step 3 — Client configuration](#4-step-3--client-configuration)
    - [Claude Code / Claude Desktop / Cline](#claude-code--claude-desktop--cline)
    - [Cursor](#cursor)
    - [Rusta](#rusta)
    - [Generic / custom harness](#generic--custom-harness)
  - [5. Step 4 — Timeouts that actually work](#5-step-4--timeouts-that-actually-work)
  - [6. Step 5 — Handling 75 tools without hallucination](#6-step-5--handling-75-tools-without-hallucination)
  - [7. Step 6 — The system prompt (copy-paste)](#7-step-6--the-system-prompt-copy-paste)
  - [8. Step 7 — Approval / permission policy](#8-step-7--approval--permission-policy)
  - [9. Workflow recipes](#9-workflow-recipes)
    - [Index, then answer a "where is X?" question](#index-then-answer-a-where-is-x-question)
    - [Navigate precisely with LSP](#navigate-precisely-with-lsp)
    - [Safe rename](#safe-rename)
    - [Remember and recall](#remember-and-recall)
    - [The graph follows your edits](#the-graph-follows-your-edits)
  - [10. Parameter reference and gotchas](#10-parameter-reference-and-gotchas)
  - [11. The Context Graph: long-term memory](#11-the-context-graph-long-term-memory)
  - [12. Troubleshooting](#12-troubleshooting)
  - [13. Integration checklist](#13-integration-checklist)
  - [Related documentation](#related-documentation)

---

## 1. What you get: the 75 tools

| Family | Count | Needs | Purpose |
| --- | ---: | --- | --- |
| **Core** | 32 | A `scan_codebase` first | Scan, search, analyze, context graph, watcher |
| **LSP** | 38 | An installed language server (started on demand) | Hover, definition, references, rename, diagnostics… |
| **Architecture** | 5 | — | Detect / check / install language servers |
| **Total** | **75** | | |

The 32 core tools break down as **24 scan/search/analyze/file** + **5 Context
Graph** + **3 watcher**.

> **Common misconception.** The Context Graph is **5 MCP tools**
> (`add_context_note`, `search_context`, `get_related_context`, `prune_context`,
> `get_context_statistics`), already counted inside the 32 core tools — *not* a
> separate 9-tool surface. The number **9** refers to the nine *note types*
> (`adr`, `standard`, `optimization`, `failure`, `practice`, `reflection`,
> `general`, `file_description`, `modification`).

---

## 2. Step 1 — Use the right binary

**This is the single most common integration failure.** The LSP and Architecture
tools are behind a **compile-time** feature gate (`#[cfg(feature = "lsp")]`).

| What you run | Tools served |
| --- | ---: |
| `devscriptor mcp` — the CLI, built with `devscriptor-cli` **default** features (`lsp` + `mcp`) | **75** ✅ |
| `devscriptor-mcp-standalone`, built from the `devscriptor-mcp` crate (its defaults **omit** `lsp`) | **32** ❌ |
| Any build with `--no-default-features` | 32 or fewer ❌ |

Rules of thumb:

- **Do not** pass `--features` when building for release. `devscriptor-cli`'s
  defaults already produce the full 75-tool universal binary.
- Build the full binary with:

  ```bash
  cargo build --release -p devscriptor-cli
  # -> target/release/devscriptor
  ```

- The only command for any MCP client: **`devscriptor` with `args: ["mcp"]`**.
  Devscriptor ships one executable answering to one name.

A 32-tool server silently omits every LSP and Architecture tool. Your agent will
"work" and simply never navigate code. Always verify (next step).

---

## 3. Step 2 — Verify the server before trusting it

Two invariants matter: **tool count** and **stdout purity**.

### 3a. Tool count

Save as `probe_mcp.py` and run `python3 probe_mcp.py devscriptor mcp`:

```python
import subprocess, json, sys, threading
argv = sys.argv[1:]
p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.DEVNULL, text=True, bufsize=1)
send = lambda o: (p.stdin.write(json.dumps(o) + "\n"), p.stdin.flush())
send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2024-11-05", "capabilities": {},
    "clientInfo": {"name": "probe", "version": "0"}}})
res = {}
def rd():
    for line in p.stdout:
        if not line.strip():
            continue
        try: m = json.loads(line)
        except Exception: continue          # a non-JSON line == broken server, see 3b
        if m.get("id") == 1:
            send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        if m.get("id") == 2:
            res["r"] = m; break
t = threading.Thread(target=rd, daemon=True); t.start(); t.join(timeout=120)
names = sorted(t["name"] for t in res["r"]["result"]["tools"])
print("TOTAL:", len(names))
print("LSP:", "get_hover" in names, "| ARCH:", "lsp_arch_setup" in names,
      "| WATCH:", "start_watching" in names)
p.kill()
```

Expected:

```bash
TOTAL: 75
LSP: True | ARCH: True | WATCH: True
```

If you see `TOTAL: 32`, re-read [Step 1](#2-step-1--use-the-right-binary).

### 3b. stdout must be pure JSON-RPC

An MCP stdio server **must never write non-protocol bytes to stdout**. A single
log line there corrupts the stream, and strict clients die with:

```bash
Error reading from stream: serde error expected value at line 1 column 1
...transport closed; reconnecting
```

Check that stdout is silent and logs go to stderr:

```bash
# stdout: must print NOTHING (no banner, no tracing lines)
devscriptor --verbose mcp 2>/dev/null | head -c 200; echo "[stdout end]"

# stderr: warnings belong here
devscriptor --verbose mcp 2>&1 >/dev/null | head -c 200
```

> Devscriptor ignores `RUST_LOG` — its tracing filter is a fixed directive. Use
> the global `--verbose` flag, which is also the loudest setting and therefore the
> strongest stdout-purity check. A single log line on stdout — such as the
> startup `WARN` its LSP layer emits when an optional language server is absent —
> would land in the middle of the JSON-RPC stream and kill the transport, so
> every log goes to stderr. If you maintain a fork, keep it that way
> (`.with_writer(std::io::stderr)` on every subscriber in the MCP paths); if you
> consume a build, verify with the command above.

Lenient clients (which skip unparsable lines) mask this bug; strict SDK-based
clients (`rmcp`, the official TS/Python SDKs) will not.

---

## 4. Step 3 — Client configuration

### Claude Code / Claude Desktop / Cline

```json
{
  "mcpServers": {
    "devscriptor": {
      "command": "devscriptor",
      "args": ["mcp"]
    }
  }
}
```

Always `devscriptor` + `["mcp"]`. Devscriptor ships a single executable.

> There is no separate `devscriptor-mcp` entry point. As a *relative symlink* it
> would dangle when the binary is installed on its own, and as a `.cmd` shim on
> Windows many spawners cannot execute it — Node's `child_process.spawn` refuses
> `.cmd` without `shell: true`, and Rust's `std::process::Command` cannot run one
> at all. If a third-party server you integrate ships such a shim, spawn it via
> `cmd /C`.

### Cursor

```json
{
  "mcpServers": {
    "devscriptor": { "command": "devscriptor", "args": ["mcp"] }
  }
}
```

### Rusta

No configuration required — Rusta discovers an installed Devscriptor and
registers it automatically, with the right timeouts. Discovery order:

1. `$DEVSCRIPTOR_BIN` (an explicit path to the `devscriptor` executable)
2. `$PATH`
3. the platform install directory: `/usr/local/bin`, `/usr/bin`,
   `/opt/homebrew/bin`, `~/.local/bin`, `~/bin`, `~/.devscriptor/bin` on
   Linux/macOS; `devscriptor` and `Programs\devscriptor` under each of
   `%LOCALAPPDATA%`, `%ProgramFiles%` and `%ProgramW6432%`, then
   `%USERPROFILE%\.devscriptor\bin` on Windows

Rusta runs the injected server in the session's workspace, with `--database`
set to the workspace's `devscriptor.db` — the index `devscriptor scan` writes — or
an existing one further up the same repository; `$DEVSCRIPTOR_DB` overrides it.
So the agent reads the index you built, not the server's shared default.

Devscriptor runs one process per workspace. The injected `devscriptor mcp`
relays to the workspace's `devscriptor serve`, one per workspace and database, and
starts it when none runs; it stops 10 minutes after its last session. The Rusta
sessions in a workspace, and other MCP clients there, therefore share one index,
watcher, embedding model and set of language servers (a session's sub-agents share
its server too): a second session does not start a rust-analyzer of its own. The
tools, their answers and the injected command are the same either way.
`health_check` reports the shared process as `daemon_pid`, and
`DEVSCRIPTOR_DAEMON=0` in Rusta's environment keeps one Devscriptor per session.

To pin a custom build or opt out, declare the server yourself in `.rusta.toml`
— an explicit entry (including `disabled = true`) always wins:

```toml
[[mcp_servers]]
name = "devscriptor"
command = "/opt/devscriptor/devscriptor"
args = ["mcp"]
startup_timeout_ms = 60000
tool_timeout_ms = 180000
```

### Generic / custom harness

- Transport: **stdio** (JSON-RPC). There is no network port.
- Discovery: call `tools/list`. Devscriptor returns all 75 tools in **one page**
  (`nextCursor: null`). Still, prefer your SDK's cursor-following variant
  (e.g. rmcp's `list_all_tools()`) so you are correct if that ever changes.
- Every tool carries a non-empty `description`, and every `inputSchema` is a
  well-formed `{"type": "object", ...}` with **no `$ref`** and explicit `type` on
  each property. That means no schema pre-processing is required for
  OpenAI-compatible, Anthropic, Kimi/Moonshot, or GLM/Z.AI tool serialization.

Namespace tool names per your client's convention (e.g. Rusta exposes them as
`mcp__devscriptor__<tool>`). Use the schema returned by `tools/list` as the
source of truth — **never** hardcode parameters from prose docs (see
[§10](#10-parameter-reference-and-gotchas)).

---

## 5. Step 4 — Timeouts that actually work

The default 60 s per-tool timeout in most clients is **too short**. Raise it.

| Operation | Why it's slow | What to do |
| --- | --- | --- |
| `scan_codebase` | Parses the whole tree | Per-tool timeout ≥ 180 s, or use `start_analysis` + `get_analysis_progress` |
| First LSP call | Boots and indexes the language server, unless it started with the MCP server | Expect a slow first call; **do not** retry immediately |
| `get_diagnostics` on a push-only server (typescript-language-server, clangd, gopls, jdtls) | The server pushes diagnostics after analysing the file, often in waves; Devscriptor waits for them to settle | Expect a few seconds; asked again about an unchanged file, it answers at once |
| `semantic_search` (cold) | Loads the embedding model | Allow ≥ 60 s on first use |
| Server startup | Starts the language servers its working directory needs (those whose project markers it finds, plus the HTML, SQL and Bash servers) before answering `initialize` | Startup timeout ≈ 60 s |

Rusta spells these `tool_timeout_ms` and `startup_timeout_ms` in `[[mcp_servers]]`,
and already applies 180 s / 60 s to its auto-detected Devscriptor server; the same
keys under a global `[mcp]` table change those. Other clients name them
differently — check yours.

For big repositories, prefer the **async job** pattern over a blocking scan:

```bash
start_analysis(path)  ->  job_id
get_analysis_progress(job_id, wait=true, timeout=30)   # poll until complete
```

---

## 6. Step 5 — Handling 75 tools without hallucination

Injecting 75 full JSON schemas into every request is the main cause of
wrong-tool selection, invented tool names, and context bloat. Pick one of these,
in order of preference:

**A. Progressive tool disclosure (best).** Advertise only tool *names +
one-line descriptions*, and let the model load full definitions on demand via a
`select_tools`-style primitive. This is what Rusta does: MCP tools register as
*dynamic*, the model receives

```bash
<tools_added>
## devscriptor
mcp__devscriptor__semantic_search — Semantic search using embeddings
mcp__devscriptor__get_hover — Get hover info (type signature/docs) for a symbol. Pass a symbol `name` (resolved from the code index) or explicit line/character.
...
</tools_added>
```

and calls `select_tools({"names": [...]})` before invoking them. Two rules make
this reliable:

- **Always include the description.** A bare list of 75 names forces the model to
  guess from the identifier alone — that is precisely where ambiguity and
  invented names come from.
- **Group by server** (`## devscriptor`) so large servers stay navigable.

**B. Tool filtering.** If your client cannot do disclosure, expose a curated
subset per task type — e.g. search + navigation for code Q&A; add the
Architecture tools only when LSP is needed.

**C. Full exposure.** Only acceptable with a large context window and a model
strong at tool selection. Expect degradation.

Whichever you choose: **validate tool names against the discovered list** and
return a clear `Unknown tool: <name>` error rather than silently failing. That
single guard converts a hallucination into a cheap, self-correcting round-trip.

---

## 7. Step 6 — The system prompt (copy-paste)

Tool schemas tell a model *how* to call a tool, never *when*. Without the block
below, agents read files blindly, call LSP tools before a language server exists,
and never use the Context Graph. Adapt the `mcp__devscriptor__` prefix to your
client's naming.

```markdown
# Devscriptor code intelligence

A Devscriptor MCP server is connected, providing 75 tools over a persistent
code knowledge graph. Prefer these tools over blind file reading or grepping:
they are indexed, cheaper, and more precise. Never invent a Devscriptor tool
name or parameter — use only the tools listed and the parameters in their schema.

## Establish the index first
`get_statistics` reports whether a codebase has been indexed. If it reports no
entities, run `scan_codebase` (parameter `path`) before searching; an index that
has entities needs no new scan. For a large repository use `start_analysis` and
poll `get_analysis_progress` instead of a long blocking call. `health_check`
reports whether embeddings and FTS5 are available. The index follows your
edits: Devscriptor watches the workspace and re-checks the files behind every
answer.

## Finding code
- `semantic_search` — meaning-based lookup ("where is config parsed?"). Embeds
  the query's keyword candidates on demand, so it works right after a scan when
  the query's words occur in the code; call `generate_embeddings` once to build
  the whole vector index, after which any wording finds what it means. On an
  index with no vectors yet, an empty answer says so in its `message` rather
  than meaning nothing relevant exists. If `health_check` says embeddings are unavailable, use
  keyword search. **Leave `entity_type` off to search every kind** — naming one
  narrows the search to it, which will hide classes, modules and documentation
  sections from an otherwise complete-looking result. Each result carries
  `file_path`, `start_line` and `end_line` (1-indexed), so a match can be cited
  as `file:line` without reading the file; `fuzzy_search` results do too. Its
  `body_preview` shows the first lines numbered as in the file
  (`<line><TAB><text>`), as are `get_file_context`'s snippets and
  `describe_file`'s `content_preview`: cite a line inside one by its number.
- `generate_embeddings` — build/refresh the semantic vector index over MCP
  (the counterpart of the `devscriptor embed` CLI command; idempotent).
- `search_functions` / `search_classes` / `search_entities` — name lookup.
- `fuzzy_search` — when the exact spelling is unknown.
- `query_with_context` — code for a query in words or by name: what the Context
  Graph notes about the query point at, else the code ranked as
  `semantic_search` ranks it.
- `get_entity_details`, `get_relationships` — inspect an entity, and what it
  calls and is called by, uses, implements or inherits. Each relationship gives
  the file and line of both ends and the lines a call is made on, so it answers
  "who calls this, where?" with no language server; `file_path` tells entities
  of one name apart.
- `find_dead_code`, `analyze_complexity` — code-quality questions.

## Precise navigation (LSP tools)
The 38 LSP tools (`get_hover`, `get_definition`, `get_references`,
`get_implementations`, `rename_symbol`, `get_diagnostics`, …) proxy a live
language server. The position tools take a symbol `name` — resolved from the
index, so the codebase must be scanned — or `file_path`, `line`, `character`
(all 0-indexed); prefer `name` to computing a column. LSP results carry `cite`
(`path:line:col`, 1-based); quote it as is. The call- and type-hierarchy tools
(`get_incoming_calls`, `get_outgoing_calls`, `get_subtypes`, `get_supertypes`)
take the symbol the same way.
They need the language's server installed; Devscriptor starts it on first use.
Before the first LSP call in a project, call `lsp_arch_detect` (parameter
`project_path`); if a needed server is missing, call `lsp_arch_setup` with
`auto_install: true`. The first LSP call after a server starts can be slow —
that is expected; do not retry immediately.

Use `get_definition` / `get_references` instead of grepping for a symbol, and
`rename_symbol` instead of hand-editing every call site. `get_references` adds,
in `index_locations`, the uses the index records that the server did not report
— in files it cannot read, such as `.vue` and `.svelte` files for the TypeScript
server — and `get_incoming_calls`, in `index_calls`, such calls with their
callers; they are matched by name, so check them. `rename_symbol`,
`format_document` and `get_range_format` write their edits to the files on disk;
re-read a file before editing it after one of them.

## Context Graph — long-term memory
Five tools persist notes across sessions. Note types: `adr`, `standard`,
`optimization`, `failure`, `practice`, `reflection`, `general`,
`file_description`, `modification`.

- At the start of non-trivial work, call `search_context`
  (`search_type: "hybrid"`) to recall what is already known. `get_related_context`
  retrieves notes linked to an entity type.
- When you make an architectural decision, discover a non-obvious constraint,
  fix a subtle bug, or are corrected by the user, record it with
  `add_context_note`: pick the matching `note_type`, set `priority` (1–10), and
  link it to code via `related_entities`. Write the *why*, not what the diff
  already shows.
- Search before adding; do not duplicate an existing note.

## Failure handling
Diagnose before retrying. When a file's language server does not run, the error
names the server and says why, and what makes it run: `lsp_arch_setup` installs
one that is not installed, while one built for another Ruby, or linked in from
elsewhere, is fixed outside the session — say so. "No LSP server available for
file" means no language server reads that file type (VB.NET, a MATLAB `.m`);
answer from the index and the file. "no project root above"
means the file is not inside a project with the manifest its server needs
(`Cargo.toml`, a `.csproj`, …). An empty search result means no match: rephrase,
or ask `semantic_search` in words — unless `get_statistics` shows an empty index,
which needs a scan. A timeout on `scan_codebase` means switch to
`start_analysis` and poll. Never repeat an identical failing call.
```

---

## 8. Step 7 — Approval / permission policy

Most agents infer an approval rule from a tool's name and default unknown MCP
tools to "ask the user." With 75 tools that produces an approval prompt on nearly
every call. Classify them instead:

**Auto-allow (read-only, or writes only to Devscriptor's own SQLite graph):**
all `search_*`, `semantic_search`, `fuzzy_search`, `get_*` **except
`get_range_format`**, `describe_*`, `analyze_*`, `find_dead_code`,
`health_check`, `validate_file_edit`, `query_with_context`, `scan_codebase`,
`start_analysis`, `generate_embeddings`, `add_context_note`,
`start_watching` / `stop_watching`.

**Prompt the user (installs software, spawns processes, or rewrites the workspace):**

| Tool | Why |
| --- | --- |
| `lsp_arch_setup` | May download and install language servers |
| `rename_symbol` | Edits every affected file on disk |
| `format_document` / `get_range_format` | Rewrites the file on disk |
| `prune_context` | Archives old notes and deletes long-archived ones |
| `start_server` / `stop_server` / `restart_server` | Spawns / kills processes |

Notes:

- `format_document`, `get_range_format`, and `rename_symbol` **write** their
  edits to disk themselves, then return the edits they applied. Gate them like
  your own file-writing tools. (Rusta classifies them as `write_file`: asked
  in the default permission mode, allowed in `accept_edits`.)
- `prune_context` defaults to `dry_run: true`. Preview, then set `dry_run: false`.
- Keep your own path sandbox enforced independently of these rules, so an
  "allow" can never let a tool escape the working directory. Note this also
  applies to *filter* arguments that merely look like paths: Rusta rejects
  `get_statistics(path="/outside/repo")` even though the parameter only scopes a
  database query. Add the directory to your agent's allowed paths when you
  genuinely need to index or query another tree.

---

## 9. Workflow recipes

### Index, then answer a "where is X?" question

```bash
get_statistics(path)                    # is it indexed?
scan_codebase(path)                     # if not (or start_analysis for big repos)
semantic_search(query="parse configuration file", limit=10)
get_entity_details(entity_type="function", name="parse_config")
get_relationships(entity_name="parse_config", file_path="src/config.rs")
                                        # what calls it, and on which lines
```

### Navigate precisely with LSP

```bash
lsp_arch_detect(project_path)           # once per project
lsp_arch_setup(project_path, auto_install=true)   # if a server is missing
get_definition(name="parse_config")     # by name, resolved from the index
get_definition(file_path, line, character)        # or by position, 0-indexed
get_references(name="parse_config", include_declaration=true)
get_incoming_calls(name="parse_config")           # callers, no hierarchy item needed
```

### Safe rename

```bash
get_references(...)                     # review the blast radius first
rename_symbol(name="parse_config", new_name="load_config")   # edits the files, returns the changes
```

### Remember and recall

```bash
search_context(query="auth token verification", search_type="hybrid", top_k=5)
# ... do the work ...
add_context_note(note_type="adr", title="...", content="why, not what",
                 priority=8, related_entities=[{"entity_type":"function",
                 "entity_name":"verify_token","file_path":"src/auth.rs"}])
```

### The graph follows your edits

Devscriptor keeps the graph current by itself. `devscriptor mcp` watches the
scanned project it runs in (`--no-watch` or `DEVSCRIPTOR_WATCH=0` turn that off),
and every answer from the index first re-checks the files it names, re-indexing
those that changed. An answer lists the files it re-indexed in `refreshed_files`,
and those that changed but were not re-indexed in time in `stale_files`: read
those from disk before relying on the answer. An index whose scan recorded no
root is neither watched nor re-indexed for answers until `devscriptor scan` scans
it again.

```bash
get_watch_status()     # watching, path, mode ("events" or "poll"), rescans
start_watching(path)   # another root, when none is watched; the watched one answers already_watching
```


---

## 10. Parameter reference and gotchas

**Always take parameters from the live `tools/list` schema.** It is the only
source that cannot drift from the server you are actually talking to, and it is
what the traps below are checked against.

Verified signatures (`*` = required):

| Tool | Parameters |
| --- | --- |
| `scan_codebase` | `path*`, **`extensions`**, `exclude_patterns` |
| `start_analysis` | `path*`, `extensions`, `exclude_patterns` |
| `get_analysis_progress` | `job_id*`, `wait`, `timeout` |
| `get_statistics` | `path` |
| `semantic_search` | `query*`, `entity_type`, `limit` |
| `get_entity_details` | `entity_type*`, `name*`, `file_path` |
| `get_relationships` | `entity_name*`, `entity_type`, `file_path`, `relationship_type` |
| `search_context` | `query*`, `top_k`, `search_type`, `note_type`, `status`, `min_priority`, `max_content_length` |
| `get_related_context` | `entity_type*`, `entity_name`, `file_path`, `max_content_length` |
| `add_context_note` | `note_type*`, `title*`, `content*`, `summary`, `tags`, `priority`, `author`, `related_entities`, `related_concepts` |
| `prune_context` | `max_age_days`, `max_notes`, `dry_run` |
| `start_watching` | `path*`, `debounce_ms`, `embed` |
| `query_with_context` | `query*`, `entity_type`, `file_path`, `force_broad_search` |
| `generate_embeddings` | `force`, `batch_size` |
| `get_hover` | `name`, `entity_type`, `file_path`, `line`, `character` † |
| `get_references` | `name`, `entity_type`, `file_path`, `line`, `character` †, `include_declaration` |
| `rename_symbol` | `name`, `entity_type`, `file_path`, `line`, `character` †, `new_name*` |
| `get_incoming_calls`, `get_outgoing_calls`, `get_subtypes`, `get_supertypes` | `item`, or the symbol as for `get_hover` † |
| `get_diagnostics` | `file_path*` |
| `start_server` | `language_id*`, `project` |
| `lsp_arch_detect` | `project_path` |
| `lsp_arch_setup` | `project_path`, `auto_install`, `servers_to_install` |

† None of these is required on its own: give `name` (with `entity_type` or
`file_path` to choose between symbols of that name), or `file_path`, `line` and
`character`. A hierarchy tool also takes an `item` from `get_call_hierarchy` /
`get_type_hierarchy`, passed back unchanged.

Traps that cause silent failures:

1. **The extension filter is `extensions`, on both `scan_codebase` and
   `start_analysis`.** A `file_extensions` argument is silently ignored, so a
   client that sends it scans every file type.
2. **Three different path parameters.** `path` (scan/analyze/watch/statistics),
   `project_path` (`lsp_arch_detect`, `lsp_arch_setup`), `file_path` (all LSP
   position tools).
3. **`search_context` uses `top_k`;** every other search tool uses `limit`.
4. **LSP positions use `character`, not `column`, and are 0-indexed.** A
   `character` counts UTF-16 code units (one per character, two for an emoji).
   A computed column is the commonest LSP mistake; pass the symbol's `name`
   instead where you can.
5. **`get_entity_details` uses `name`; `get_related_context` uses `entity_name`.**
6. **A hierarchy `item` must go back whole.** Copied back field by field, an
   item that loses `location.range.end` or `selection_range` is refused or
   finds nothing. Name the symbol instead and the hierarchy is prepared for
   you.
7. **`start_server` takes `project`:** a path in the project to start the
   server for. Without it the server starts for the workspace; a Rust, C#, F#
   or Scala server, which needs its manifest at its root, starts for the
   projects inside a workspace root that is not one itself.

If your client can remap arguments, alias these both ways (`path` ↔
`project_path` ↔ `file_path`, `column` → `character`, `limit` ↔ `top_k`,
`file_extensions` → `extensions`, `name` → `entity_name`) — but **never overwrite
a property the model already supplied correctly**, and **remap before you check
a call against the schema**: checked first, a required property under another
name is refused before any alias applies. The `file_extensions` alias is purely
defensive: no tool declares it, but models still emit it.

---

## 11. The Context Graph: long-term memory

Five tools, backed by the same SQLite database as the code graph, persisting
across sessions and processes. Nothing is written automatically — a `scan` never
creates notes. Notes are only created by explicit `add_context_note` calls.

- **Note types (9):** `adr`, `standard`, `optimization`, `failure`, `practice`,
  `reflection`, `general`, `file_description`, `modification`.
- **Search strategies** (`search_type` on `search_context`): `keyword` (default),
  `semantic` (needs embeddings; falls back to keyword), `hybrid` (both, fused
  with Reciprocal Rank Fusion). **Use `hybrid` for recall.**
- **Linking:** `related_entities` (`entity_type` required; `entity_name`,
  `file_path`, `entity_id`, `link_type` optional) and `related_concepts`
  (`concept_type` + `concept_name`). Links survive re-scans as long as the names
  persist.
- **Pruning** archives active notes not updated within `max_age_days` (and the
  lowest-ranked surplus over `max_notes`), and deletes notes that have stayed
  archived for more than twice `max_age_days`. `dry_run` defaults to `true`.

To make an agent genuinely stateful, put two instructions in the system prompt:
*search the context graph before starting non-trivial work*, and *record decisions,
constraints, and corrections as notes with the why*. Without those, the Context
Graph stays empty and the integration is only half of what it can be.

---

## 12. Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| Only 32 tools; no `get_hover` | Running `devscriptor-mcp-standalone`, or a build without the `lsp` feature | Use `devscriptor mcp` from a default-feature `devscriptor-cli` build |
| `serde error expected value at line 1 column 1`, `transport closed` | Server wrote a log/banner to **stdout** | Route logs to stderr (`.with_writer(std::io::stderr)`); verify per [§3b](#3b-stdout-must-be-pure-json-rpc) |
| Model says "I don't have those tools" | Agent started its first turn before MCP discovery finished | Wait for servers to connect **and** tools to register before the first turn |
| `the <language> language server (<id>) for .ext files is not installed: …` | Its server is not installed | `lsp_arch_setup(auto_install=true)`, as the message says |
| `the <language> language server (<id>) for .ext files was built for Ruby …` (or is another build, or holds no binary) | Its install cannot run here; `lsp_arch_status` lists it under `cannot_run`, with the same `reason` | Do what the message says; a server linked in from elsewhere is rebuilt where the link points, not by `lsp_arch_setup` |
| `No LSP server available for file: No LSP server for extension: .ext` | No language server reads that file type (VB.NET, a MATLAB `.m`) | Answer from the index and the file |
| `No LSP server configured for language: rust: no project root above …` | A Rust, C#, F# or Scala file with no manifest (`Cargo.toml`, `.csproj`, …) at or above it | Ask about a file inside a project that has one |
| Empty search results | Codebase never scanned, or a sentence given to a name lookup (`search_*`) | `get_statistics`, then `scan_codebase` if it shows no entities; ask a question in words with `semantic_search` |
| `semantic_search` falls back / fails | Embeddings unavailable | `health_check`; call `generate_embeddings` (or run `devscriptor embed`); ensure the embedding model is downloaded |
| LSP tools ignore the project's `devscriptor-lsp.toml` (stderr: `Ignoring ./devscriptor-lsp.toml`) | The file names commands to run, so Devscriptor uses it only once trusted | Review it, then run `devscriptor lsp trust` in that directory |
| Search previews show code the agent has already changed | The file changed but was not re-indexed in time for the answer | Read the files the answer lists in `stale_files` from disk; `refreshed_files` names those re-indexed for it |
| `scan_codebase` times out | Your client's per-tool timeout is too short for a first scan | Raise it (Rusta: `tool_timeout_ms`, which defaults to 60 s but is set to 180 s for Devscriptor — under `[mcp]` for an auto-detected one), or use `start_analysis` + `get_analysis_progress` |
| Approval prompt on every tool call | Client defaults unknown MCP tools to "ask" | Apply the policy in [§8](#8-step-7--approval--permission-policy) |
| Model invents a tool name | Names advertised without descriptions | Include one-line descriptions; validate names against the discovered list |
| A parameter you pass has no effect | It is not in the tool's live schema (a misspelling, or a name from prose docs) | Take parameters from `tools/list`; verify with a behaviour probe, e.g. `get_statistics` with a bogus `path` must return zeros |

---

## 13. Integration checklist

- [ ] `tools/list` returns **75** tools, including `get_hover`, `lsp_arch_setup`, `start_watching`.
- [ ] The server starts on a machine with **no language servers installed** (core + Architecture tools must remain usable so the agent can install them).
- [ ] Server stdout is byte-for-byte pure JSON-RPC; all logs go to stderr.
- [ ] Client follows pagination cursors (or tolerates a single page).
- [ ] Per-tool timeout ≥ 180 s, startup timeout ≈ 60 s.
- [ ] Agent waits for tool registration before its first turn.
- [ ] 75 tools are exposed via progressive disclosure **with descriptions**, not 75 raw schemas.
- [ ] The Devscriptor system-prompt block ([§7](#7-step-6--the-system-prompt-copy-paste)) is installed.
- [ ] Approval policy distinguishes read-only from mutating tools ([§8](#8-step-7--approval--permission-policy)).
- [ ] Parameters come from the live schema, not from prose docs ([§10](#10-parameter-reference-and-gotchas)).
- [ ] The agent searches the Context Graph before, and writes notes after, non-trivial work.
- [ ] Answers follow the files the agent edits, so later searches do not show the code as it was: Devscriptor does this by itself and names re-checked files in `refreshed_files` / `stale_files`.
- [ ] A behaviour probe confirms parameters are honoured, not just accepted (e.g. `get_statistics` with an unmatched `path` returns zeros).
- [ ] Smoke test passes: the model calls `select_tools` (or equivalent), then a real tool, and reports its output.

---

## Related documentation

- Devscriptor: `docs/MCP_TOOLS.md` (all 75 tools), `docs/CONTEXT_GRAPH.md`,
  `docs/LSP_README.md`, `docs/INSTALL.md`.
- Rusta's implementation: `crates/mcp-client/` (discovery, arg remap),
  `crates/agent-core/src/tool_disclosure.rs` (announcements),
  `crates/rusta-cli/src/devscriptor.rs` (system-prompt guidance),
  `.rusta.toml` (config + permission policy).
