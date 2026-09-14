# Rusta

A lean, lightweight, all-encompassing AI coding-agent harness for small, locally
hosted coding LLMs (8B–35B parameters). Rust + Tokio, one fast binary.

> **Status: pre-alpha.** Development is driven milestone-by-milestone by
> [`DEVELOPMENT_PLAN.md`](DEVELOPMENT_PLAN.md) (v1.2) — the single source of truth.
> **v1 is milestone-complete: M0–M8 all green**, plus the 2026-09-14 third-audit
> remediation (clippy `-D warnings` clean, `cargo doc` 0 warnings; counts below
> are produced by `scripts/loc_budget.sh` and `cargo test --workspace`).
> The subsystem trail: **M0 — workspace skeleton** (8 crates, CI, LoC-budget gate), **M1 —
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
> ranking, identifier word-scan backfill for def-only languages, definition-level
> token-budget-fitted rendering with `⋮` elision markers, mtime-keyed in-memory tag
> cache, and the `map_drill` definition-span (±8-line context padding) / exact
> line-window API), and
> **M5 — `rusta-core` context manager** (JIT skill cards — a starter deck compiled
> into the binary, extensible per project from `.rusta/skills/*.md` — with a
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
> green), and **M7 — `rusta-dispatch` + `rusta-tools`** (the ten-tool phase-gated
> registry: every state × tool cell pinned by a matrix test — mutation is
> physically unreachable in read-only states, blocked calls answer with the
> 1–2-line corrective note; §6.1 result caps everywhere; `edit`/`write` funnel
> into the same apply chain, undo journal, and read-before-edit ledger as
> SEARCH/REPLACE blocks; §6.12 shell policy with a deny regex table, minimal
> `PATH/HOME/LANG` environment, timeout kill, and injected approval; plus
> isolated read-only sub-coders — up to four parallel research tasks in fresh
> contexts with a 6-turn budget, only their labeled ≤ 400-token reports
> re-entering the main context, and requests serialized on the embedded
> backend's single inference thread), and **M8 — `rusta-cli`** (the Aider-style
> host: the §6.1 turn lifecycle with document-order tool/edit execution, native
> `tool_calls` passthrough, turn cap with wrap-up, Ctrl-C abort; the §6.4 plan
> gate (y/n; `/auto` approves); §6.7 Reflexion repair rounds; §6.9 git
> auto-commit per applied batch (`rusta: <summary>`) and `/undo` that restores
> files via the journal and reverts the commit — never an unrelated one;
> `rusta.toml` discovery (cwd → parents → `~/.rusta/`) with flag-over-file
> precedence; append-only sessions at `~/.rusta/sessions/`; a reedline REPL
> with all 13 slash commands incl. `/resume` full-state replay; and the
> non-interactive `-c` mode where shell is denied by default). The default
> artifact stays cmake-free — no llama.cpp build unless you ask for it.

## Quickstart

```sh
cargo build --release

# point at any OpenAI-compatible server (llama-server :8080/v1, Ollama :11434/v1, LM Studio :1234/v1)
cp rusta.toml.example rusta.toml   # edit [backend] base_url + [validate] commands

./target/release/rusta            # interactive REPL — /help lists commands
./target/release/rusta -c "fix the failing test in src/lib.rs"   # one shot, then exit
cargo build --release --features embedded   # rusta-full: in-process llama.cpp
```

The REPL runs the full loop: model turns stream live, tool calls and
SEARCH/REPLACE edits execute through the phase-gated registry, each applied
batch auto-commits and runs the configured validators, red rounds feed the
model a capped repair report before you see anything, and `/undo` reverts the
last batch (file + commit). Sessions are append-only JSONL under
`~/.rusta/sessions/` — `/resume <file>` continues where you left off.

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
  `llama-server`, Ollama, LM Studio, vLLM. `base_url` must be the full API root
  including `/v1`.
- **Embedded** (opt-in): in-process llama.cpp via `cargo build --features embedded`.
  Needs cmake and a C++ toolchain; the default artifact stays cmake-free.

## Choosing a model (plan §13)

Rusta targets 8B–35B coding models. Pick by the VRAM you actually have — a model
that spills to system RAM will dominate your turn latency far more than the
scaffold does.

| Budget | Suggested models |
| ------ | ---------------- |
| **24 GB VRAM** | Qwen3-Coder 30B-A3B · Qwen3.6 27B MTP · Gemma 4 31B IT QAT |
| **12–16 GB VRAM** | gpt-oss-20b · DeepSeek-R1 14B · Gemma 4 12B |
| **≤ 8 GB VRAM** | Qwen3.5 9B MTP |
| **CPU only** | 4-bit MoE A3B models via llama.cpp, with `-t` set to your *physical* core count |

On the embedded backend one loaded model shares one inference thread, so
sub-coder dispatch (§6.8) serializes; true parallel research needs the HTTP
backend against a server that batches.

## License

MIT — see [LICENSE](LICENSE).
