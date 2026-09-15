# Tag queries

Tree-sitter `.scm` tag queries for the repo map (ADR.md §6.5, step 2).
Files are copied **verbatim** from the local Aider reference tree
(`/home/vladimir/develop/refs/aider/aider/queries/`) — the
`tree-sitter-language-pack` variants for c, cpp, go, javascript, python, rust,
and the `tree-sitter-languages` variant for typescript (Aider's own fallback
when the pack ships no typescript query). Aider's queries are adapted from
<https://github.com/Goldziher/tree-sitter-language-pack> (per-language licenses
listed there); Aider itself is Apache-2.0.

Only `@name.definition.*` / `@name.reference.*` captures are consumed by
`rusta-repomap`; `@doc` captures and `#strip!` / `#select-adjacent!` /
`#set-adjacent!` directives are inert for us (exactly as they are for Aider's
Python bindings), while `#not-eq?` / `#not-match?` predicates are evaluated
automatically by the tree-sitter 0.25 query cursor.

Each query is compile-tested against its pinned grammar crate in
`src/lang.rs` — the grammar-drift guard (ADR.md §13).

Data files in this directory are reported separately by
`scripts/loc_budget.sh` and never counted against the LoC budget.
