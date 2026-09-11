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
> journal, and phase from the JSONL log plus its hash-keyed diffs sidecar), and
> **M4 — `rusta-repomap`** (tree-sitter repo map: Aider `.scm` tag queries for
> seven languages over pinned grammars, def/ref graph with personalized PageRank
> ranking, identifier word-scan backfill for def-only languages, middle-drop
> token-budget-fitted rendering, mtime-keyed in-memory tag cache, and the
> `map_drill` definition-span (±8-line context padding, shared with the map's
> render constant) / exact line-window API), and
> **M5 — `rusta-core` context manager** (JIT skill cards from `skills/*.md` with a
> strict little-coder front-matter schema and ≤ 120-token bodies, ≤ 2 cards injected
> per request as a trailing system note; episodic history compression past 60% of the
> context window that keeps the last three turns, every edit block, and every error
> exchange verbatim — with `Summary`-event replay sharing the exact live code path;
> FAMA-lite loop mitigation: four detectors over canonical `tool|args` fingerprints
> and validator outputs, budget-capped mitigation capsules, and Editing → Planning
> escalation at double thresholds), and **M6 — `rusta-validate`** (the Reflexion
> validation gate: config-driven `sh -c` validators with per-command wall-clock
> timeout and capped output; model-facing feedback that is deduplicated,
> first→last-error windowed, and hard-capped at 30 lines with clickable
> `file:line:col` locations; a three-repair bound before surfacing to the user,
> wired to the phase machine — green exits `Verifying → Exploring`, red regresses
> `→ Editing`; zero-test detection attaches a write-real-tests capsule even on
> green). The default artifact stays cmake-free — no llama.cpp build unless you
> ask for it.

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
