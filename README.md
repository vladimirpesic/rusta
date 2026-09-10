# Rusta

A lean, lightweight, all-encompassing AI coding-agent harness for small, locally
hosted coding LLMs (8B–35B parameters). Rust + Tokio, one fast binary.

> **Status: pre-alpha.** Development is driven milestone-by-milestone by
> [`DEVELOPMENT_PLAN.md`](DEVELOPMENT_PLAN.md) (v1.1) — the single source of truth.
> Shipped so far: **M0 — workspace skeleton** (8 crates, CI, LoC-budget gate) and
> **M1 — `HttpBackend`** (SSE streaming, 3-attempt retry/backoff, 404 `base_url`
> hints, native `tool_calls` passthrough, mock-server e2e) plus **JSONL session
> persistence** (§6.10).

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
