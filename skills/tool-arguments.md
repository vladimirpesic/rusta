---
name: tool-arguments
type: tool
triggers: [bad_tool_args, past_eof]
priority: 8
token_cost: 95
user-invocable: false
---
# TOOL ARGUMENTS ARE TYPED

- `from`/`to` are **integers**, 1-based and inclusive. Never an identifier.
- To find a definition by name use `map_drill(path, name)` — not a line guess.
- A window past the end of a file is an error, not an empty result: read a
  window inside it, or drill the name.
- `path` is always repo-relative. No absolute paths, no `..`.
