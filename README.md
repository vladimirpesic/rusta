# Rusta

A lean, lightweight, all-encompassing AI coding-agent harness for small, locally
hosted coding LLMs (8B–35B parameters). Rust + Tokio, one fast binary.

> **Status: pre-alpha.** Development is driven milestone-by-milestone by
> [`DEVELOPMENT_PLAN.md`](DEVELOPMENT_PLAN.md) (v1.1) — the single source of truth.
> Shipped so far: **M0 — workspace skeleton** (8 crates, CI, LoC-budget gate), **M1 —
> `HttpBackend`** (SSE streaming, 3-attempt retry/backoff, 404 `base_url` hints, native
> `tool_calls` passthrough, mock-server e2e) plus **JSONL session persistence** (§6.10),
> **M1.5 — `EmbeddedBackend`** (in-process llama.cpp via `llama-cpp-2` behind the opt-in
> `embedded` cargo feature: exact token counts, model chat template with ChatML fallback,
> one serialized inference thread streaming over tokio mpsc, cancellation flag; verified
> end-to-end against a real GGUF), **M2 — `rusta-edit`** (forgiving SEARCH/REPLACE
> parser, Aider-proven apply chain with `...` elision and cross-file retry, failure
> feedback contract, undo journal, read-before-edit ledger), and **M3 — `rusta-core`**
> (phase-gated state machine `Exploring → Planning → Editing → Verifying` with an
> exhaustively pinned transition table and phase-gated tool registry, sub-500-token
> core prompt compiler, and `/resume` replay reconstructing messages, ledger, undo
> journal, and phase from the JSONL log plus its hash-keyed diffs sidecar). The
> default artifact stays cmake-free — no llama.cpp build unless you ask for it.

## Why

Small local models are not bad at coding — they are bad at managing scaffolds built
for frontier models. Rusta combines five proven scaffold pillars (Aider, little-coder,
Observer, smallcode, SmallCTL — see plan §3) into one harness: forgiving plain-text
SEARCH/REPLACE edits, tree-sitter repo-map context compression, a phase-gated state
machine, isolated sub-coder dispatch, and a sub-500-token core prompt.

## Build

```sh
cargo build --release
cargo run -p rusta-cli -- --version
```

Two backends (plan §6.2), selected at runtime in `rusta.toml` or via `--backend`:

- **HTTP** (default, always compiled): any OpenAI-compatible server — llama.cpp
  `llama-server`, Ollama, LM Studio, vLLM.
- **Embedded** (opt-in): in-process llama.cpp via `cargo build --features embedded`.

## License

MIT — see [LICENSE](LICENSE).
