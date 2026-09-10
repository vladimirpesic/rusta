# Rusta — Development Plan

**Project:** Rusta — a lean, lightweight, all-encompassing AI coding agent harness for small, locally hosted coding LLMs (8B–35B parameters).
**Version:** 1.1 (implementation-ready; revised after line-by-line review of the reference sources) · **Date:** 2026-09-10 · **Status:** Approved — implementation underway (M0 ✅, M1 ✅, M1.5 ✅)

**Specification sources (authoritative):**

- `/home/vladimir/Desktop/Optimal_LLM_Scaffold.pdf` — "LLM Scaffold Architecture Blueprint" (winning principles + Rust blueprint)
- `/home/vladimir/Desktop/rusta.txt` — LLM response corpus: framework analyses, community/benchmark evidence, hardware & model landscape (Sept 2026)
- Local reference implementations (see §3) — key mechanisms verified by direct source reading during the v1.1 revision

---

## 0. How to Use This Document (implementation contract)

This is the single source of truth for building Rusta v1. It is written to be executed by an
LLM-driven or human developer **without access to any other context**.

1. Implement milestones (§8) strictly in order; each is independently buildable and testable.
2. Sections §6.1–§6.12 are **normative specifications**. Where a subsystem cites a reference file
   in §3, read that file first; borrow its *semantics*, not its code or host language.
3. When this document is silent, defer to the cited reference implementation's behavior. When
   this document conflicts with a reference, **this document wins**.
4. Items marked **DECIDED** are final — do not re-litigate them during implementation.
5. Do not add dependencies beyond §10, tools beyond the §6.4 registry, or crates beyond §5.
6. R1 (line budget) is contractual: production ≤ 15k lines, total ≤ 20k, counted by the §12
   script (`wc -l` on `*.rs`). Tests are part of the deliverable and count against the total.
7. Every milestone's acceptance criteria must pass before starting the next. Feature completeness
   is audited against the §15 traceability matrix; §13 is the Definition of Done.
8. Rust snippets in this document are **illustrative sketches, not compile targets** — adapt
   signatures and paths to the actual crates, but preserve the stated semantics exactly.

## 1. Mission & Design Philosophy

Small local models are not bad at coding — they are bad at managing scaffolds built for frontier
models. Standard harnesses fail on 8B–35B models through context exhaustion, JSON hallucination,
and task-loop derailment ("trained monkey syndrome"). Evidence: swapping *only the scaffold* lifted
a 9B Qwen from **19.1% → 45.6%** on Aider Polyglot; SmallCode reports **87%** internal success on
a 4B-active model; a 35B model in these harnesses performs like an early Claude 3 Sonnet.

Rusta combines the five proven scaffold pillars into one fast Rust/Tokio binary:

1. **Forgiving Mutations** (Aider + smallcode) — plain-text SEARCH/REPLACE for edits, never JSON;
   a multi-format recovery parser that survives missing fences and stray markdown.
2. **Context Compression** (Aider + SmallCTL) — tree-sitter AST repo-map instead of raw file
   reads; episodic summarization of past turns.
3. **State Machine Guardrails** (SmallCTL) — strict Explore → Plan → Write lifecycle; the tool
   registry filters itself by state; mutation tools are physically unreachable in read-only phases.
4. **Actor/Dispatch Pattern** (little-coder + Observer) — isolated Tokio sub-coders for research;
   only summarized answers re-enter the main context.
5. **Context Purity & JIT Prompts** (little-coder) — core system prompt < 500 tokens; Just-In-Time
   skill-card injection triggered by intent and errors.

## 2. Hard Requirements (v1)

| # | Requirement | Notes |
| ----- | ------------- | ------- |
| R1 | **Lean budget: ≤ 15,000 lines of production Rust; ≤ 20,000 total including tests** | Counted on `*.rs` by the §12 `wc -l` script (tokei optional cross-check); enforced as a CI gate. Query files (`.scm`), skill cards (`.md`), prompt templates are *data*, tracked separately. |
| R2 | **Dual LLM backend, user-selectable at runtime** | (a) OpenAI-compatible HTTP client (llama.cpp `llama-server`, Ollama, LM Studio, vLLM) — always compiled, the default; (b) **embedded llama.cpp** via `llama-cpp-2` behind the `embedded` cargo feature. Selection via `rusta.toml` `[backend] kind = "http" \| "embedded"` or `--backend`. |
| R3 | CLI-first: Aider-style REPL | TUI / IDE extensions are out of scope for v1 (parking lot, §14). |
| R4 | Plain-text edit protocol with forgiving parser and Aider's proven apply chain | Spec §6.3. |
| R5 | Phase-gated state machine + read-before-edit enforcement | Spec §6.4. |
| R6 | Tree-sitter repo-map, token-budget fitted, incrementally cached | Spec §6.5. |
| R7 | < 500-token core prompt; JIT skill cards; history compression; loop mitigation capsules | Spec §6.6. |
| R8 | Instant auto-validation: compiler/linter feedback loop, errors fed back to the model before the user sees results | Spec §6.7. |
| R9 | Sub-coder dispatch over Tokio (isolated contexts, summarized returns) | Spec §6.8. |
| R10 | `#![forbid(unsafe_code)]` in all core crates; the only permitted `unsafe` lives behind `llama-cpp-2` in the embedded backend module. |
| R11 | Works offline, no telemetry, no accounts, no cloud calls unless the user explicitly configures an HTTP endpoint. |

## 3. Reference Repositories (cloned locally — study, port the essence, not the bulk)

Measured sizes exclude tests/benchmarks/node_modules (production source only):

| Repo | Path | Lang | Size | What Rusta extracts | Key files |
| ------ | ------ | ------ | ------ | -------------------- | ---------- |
| **Aider** | `/home/vladimir/develop/refs/aider/` | Python | ~25.1k | SEARCH/REPLACE block format, apply fallback chain, failure-feedback contract; repo-map ranking + tree-sitter queries; git auto-commit UX; `/commands` UX | `aider/coders/editblock_coder.py` (657 L), `aider/coders/udiff_coder.py` (429 L), `aider/repomap.py` (867 L), `aider/queries/*.scm` |
| **little-coder** | `/home/vladimir/develop/refs/little-coder/` | TypeScript | ~18.6k | JIT skill-card architecture + card schema; **canonical tool-call format** (fenced `tool` blocks); sub-coder Dispatch semantics; strict read-before-edit rule | `skills/tools/{read,edit,write,grep,glob,bash,dispatch}.md`, `skills/knowledge/*.md` |
| **Observer** | `/home/vladimir/develop/refs/Observer/` | TS + Rust | ~81.5k (bulk = web app — ignore) | Actor-model orchestration of specialized ephemeral agents (Planner/Coder/Reviewer); Rust CLI conventions | `cli/src/{runner,preflight,config,notify}.rs` (clap 4 + tokio + reqwest 0.12 — validates our stack) |
| **smallcode** | `/home/vladimir/develop/refs/smallcode/` | JavaScript | ~33k | Conservative drop-on-malformed parsing policy; compound-tool rationale (`read_and_patch`) | `src/tools/liquid_tool_parser.js` (314 L), `ARCHITECTURE.md` |
| **SmallCTL** | `/home/vladimir/develop/refs/SmallCTL/` | Python | ~167k (bulk = evals/vendor — ignore) | Phase contracts with per-phase blocked tools; write-session FSM; guards; FAMA loop detection + mitigation capsules; Reflexion gate | `src/smallctl/phases.py` (110 L), `guards.py` (353 L), `write_session_fsm.py` (310 L), `src/smallctl/fama/{capsules,detectors}.py` |

**Reference insight for R1:** the load-bearing mechanisms are *tiny* — Aider's entire edit engine is
657 lines, its repo-map 867; smallcode's parser 314; SmallCTL's whole phase/guard/FSM core ~1.1k.
The five-figure totals come from web UIs (Observer), eval harnesses (SmallCTL), TUI/LSP/RAG
subsystems (smallcode), and multi-provider model matrices (Aider) — all of which Rusta excludes.

**Review notes (v1.1, from direct source reading):** (a) Aider's apply chain *deliberately
disables* edit-distance matching — it fails with rich feedback instead (§6.3 adopts this);
(b) smallcode's `liquid_tool_parser` is a per-model tool-call adapter with a conservative
drop-on-failure policy — it informed Rusta's canonical tool-call format (§6.1, little-coder
lineage) and its malformed-input policy; (c) SmallCTL's capsules are one-line imperatives with
explicit next actions, budget-capped — §6.6 ports the mechanics verbatim in spirit.

## 4. Feasibility Verdict for R1 (≤ 15–20k lines of Rust)

**Verdict: DOABLE — with the scope fences below.** Budget (targets enforced per crate):

| Crate | Responsibility | Production LoC | Test LoC |
| ------- | ---------------- | ---------------: | ---------: |
| `rusta-llm` | `Backend` trait, HTTP client (SSE stream), embedded llama.cpp, token estimation, sampling config | 2,000 | 800 |
| `rusta-edit` | forgiving parser + apply chain + failure-feedback contract + read-before-edit ledger | 1,200 | 1,500 |
| `rusta-core` | state machine, session persistence, context manager, prompt compiler, loop mitigation | 2,000 | 600 |
| `rusta-tools` | phase-gated registry, 10 canonical tools (§6.4), read-before-edit ledger integration | 1,600 | 500 |
| `rusta-repomap` | tree-sitter extraction, ranking, budget-fitted rendering, cache | 1,000 | 300 |
| `rusta-dispatch` | sub-coder actors, parallel read-only DAG | 600 | 200 |
| `rusta-validate` | config-driven validators, feedback formatting | 400 | 150 |
| `rusta-cli` | REPL, `/commands`, render, git integration, config loading | 1,800 | 400 |
| glue | shared error types, small utils | 400 | 100 |
| **Total** | | **11,000** | **4,550** |

Grand total ≈ **15.5k lines** — inside the 15–20k envelope with ~4.5k of headroom. The embedded
backend is *feature-gated*; the default lean build stays in the ~10k range.

**Scope fences that make this possible (NON-goals for v1):** no TUI, no IDE plugin, no LSP
integration, no RAG/embedding store, no MCP, no multi-cloud provider matrix, no in-tree benchmark
suite, no plugin system, no web UI, no remote/SSH orchestration, no fine-tuning tooling. Anything
on this list that later proves essential goes into a *separate* opt-in crate — never the core.

## 5. Workspace Layout

```
rusta/
├── DEVELOPMENT_PLAN.md      ← this document
├── Cargo.toml               ← workspace; features: embedded, embedded-cuda (opt-in)
├── rusta.toml.example       ← both backend configs documented
├── scripts/loc_budget.sh    ← R1 enforcement gate (§12)
├── crates/
│   ├── rusta-llm/           ← Backend trait + http.rs + embedded.rs (cfg-gated) + tokens.rs
│   ├── rusta-edit/          ← parser.rs, apply.rs, ledger.rs (read-before-edit)
│   ├── rusta-core/          ← state.rs, session.rs, context.rs, prompt.rs, loops.rs
│   ├── rusta-tools/         ← registry.rs, tools/{read,grep,glob,map,map_drill,dispatch,ask,edit,write,shell}.rs
│   ├── rusta-repomap/       ← extract.rs, rank.rs, render.rs, cache.rs + queries/*.scm (data)
│   ├── rusta-dispatch/      ← actor.rs, dag.rs
│   ├── rusta-validate/      ← validators.rs
│   └── rusta-cli/           ← main.rs, repl.rs, commands.rs, render.rs, git.rs, config.rs
├── skills/                  ← JIT skill cards as markdown (data, versioned)
└── tests/                   ← e2e: mock-server fixtures, parser corpora
```

## 6. Normative Design — Subsystems

### 6.1 The Agent Loop, Tool-Call Wire Format & Turn Lifecycle

Rusta does **not** use the OpenAI `tools` parameter. The model emits plain text containing
fenced tool calls and/or SEARCH/REPLACE edit blocks; Rusta parses, executes, and re-prompts.

**Canonical tool-call format** (little-coder/`pi`-harness lineage — proven with 9B models; JSON is
allowed *only* here, as trivial two-key objects — **never for edits**):

~~~
```tool
{"name": "read", "input": {"path": "src/main.rs"}}
```
~~~

Accepted forms, in priority order: (1) canonical fenced ` ```tool ` blocks, one call per block;
(2) fenced blocks containing a JSON **array** of calls; (3) native OpenAI `tool_calls` array
passthrough (HTTP servers that emit it — parsed transparently). Unknown tool `name` → corrective
note listing the valid tools for the current state. Unparseable block → treated as prose plus a
corrective note (smallcode's conservative drop-on-failure policy). **DECIDED:** Rusta never sends
`tools`/`functions` request parameters — the syntax cheat-sheet lives in the <500-token core
prompt (context purity).

**Turn lifecycle** (one turn = one model completion):

1. Stream the completion; parse tool calls and edit blocks incrementally (parser emits each item
   as soon as its closing marker arrives).
2. If the completion contains ≥ 1 actionable item → execute in document order (edit blocks via
   §6.3, tools via §6.4) → append observations (below) → start the next turn.
3. A completion with no tool calls and no edit blocks ends the agent turn; its content is the
   answer shown to the user.
4. Hard cap: 16 model turns per user request (configurable). On cap, inject a wrap-up capsule
   ("summarize what was done, what remains, and stop") and return control to the user.
5. Ctrl-C aborts the stream, discards the partial turn, preserves session state.

**Observation contract** — tool results are appended as `user`-role observation blocks:

    TOOL RESULT read (ok)
    <content, truncated>

Hard truncation caps: read 2,000 lines / 64 KiB; grep 200 matches; glob 1,000 entries; shell
16 KiB combined stdout+stderr; dispatch 400 tokens (§6.8); repo-map as rendered (§6.5). On
truncation append `… [truncated N lines/bytes]`. Errors are returned as actionable text (§6.11),
never as panics or stack traces.

### 6.2 Backends (`rusta-llm`) — dual, runtime-selectable (R2)

```rust
#[async_trait-free via enum dispatch]
pub enum Backend { Http(HttpBackend), #[cfg(feature = "embedded")] Embedded(EmbeddedBackend) }
impl Backend {
    pub async fn stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>>;
    pub async fn complete(&self, req: ChatRequest) -> Result<String>;          // non-streaming
    pub fn count_tokens(&self, text: &str) -> u64;
    pub fn context_window(&self) -> u64;
}
```

**`HttpBackend`** — always compiled, default. `POST {base_url}/chat/completions` with
`stream: true`; SSE parsing via `reqwest` + `futures::StreamExt`; incremental `delta.content`,
finish-reason, and native `tool_calls` deltas assembled into complete calls. Retries: 3 attempts,
exponential backoff (250 ms → 2 s), connection errors and 5xx only. **Context window:** from
`[model] context_window` config, default 32768 — all budgeting uses this number. **`base_url`
must be the full API root including `/v1`**; a 404 on `/chat/completions` produces an error hint
listing correct forms for common servers (llama-server `:8080/v1`, Ollama `:11434/v1`, LM Studio
`:1234/v1`). `complete()` uses `stream: false` (used for summaries and sub-coder wrap-ups).
**Token estimation:** no tokenizer dependency — `tokens.rs` heuristic `ceil(chars / 3)`
(deliberately conservative; over-estimates prose), acceptable because every budget threshold
(§6.5 map fitting, §6.6 60% compression trigger, §6.8 report caps) carries headroom; the function
is the single swap-in point for a real tokenizer later.

**`EmbeddedBackend`** (feature `embedded`, file `embedded.rs`) — loads a GGUF via `llama-cpp-2
=0.1.156` (pinned exact: the crate ignores semver), applies the model's chat template, samples
with `LlamaSampler` (temp 0.2, top_p 0.9 defaults, configurable), and runs inference on a
dedicated OS thread streaming tokens over `tokio::sync::mpsc` (the C-API blocks; the async
boundary stays clean). Context window and exact token counts come from the loaded model
(`context_length()`, tokenizer) — no estimation when embedded. Cancellation: a shutdown flag
checked between tokens. Optional strict GBNF grammar (config `strict_grammar = true`) makes
malformed edit blocks ungeneratable — opt-in because it constrains reasoning prose too.

**Both backends** consume the same `ChatRequest { messages, max_tokens, stop, temperature }`
and are selected by config or `--backend`; the choice is invisible to every layer above.

### 6.3 Edit Protocol (`rusta-edit`) — "Just Text" mutations (R4)

Canonical block (Aider format — small models know it from training corpora):

    path/to/file.rs
    <<<<<<< SEARCH
    exact existing lines
    =======
    replacement lines
    >>>>>>> REPLACE

**Parser** (single pass over `splitlines(keepends)`; HEAD/DIVIDER/UPDATED markers matched on
`.trim()`ed lines):

1. `<<<<<<< SEARCH` (HEAD) … `=======` (DIVIDER) … `>>>>>>> REPLACE` (UPDATED). DIVIDER ends
   SEARCH collection; UPDATED **or a new DIVIDER** ends REPLACE collection (the latter
   immediately starts the next block — models sometimes chain blocks without UPDATED).
2. **New file:** HEAD immediately followed by DIVIDER (empty SEARCH) → create-file edit. A
   full rewrite of an existing file uses the `write` tool (§6.4), never an edit block.
3. **Filename resolution**, in order: scan up to 3 lines above HEAD, skipping fences/``` lines
   (DeepSeek-style fenced filenames accepted) → candidates matched against the session file-set:
   exact path → basename → fuzzy (similarity ≥ 0.8) → any candidate containing a dot. If no
   candidate and a previous block in the same response named a file → continuation (reuse it).
   Otherwise fail with the missing-filename corrective error (includes fence example).
4. Forgiveness: stray markdown fences around HEAD/UPDATED stripped; CRLF normalized; missing
   final UPDATED at end-of-stream still commits the block.
5. A fenced ` ```bash/sh/shell ` block that is **not** part of an edit is surfaced to the user as
   a *suggested command* requiring confirmation — never auto-executed.
6. The parser never panics; malformed input degrades to prose + corrective note.

**Apply chain** (per block, strictly in this order — port of Aider's *proven* sequence.
**DECIDED:** no edit-distance matching — Aider deliberately disables its fuzzy-edit-distance
path; for small models a clear retry request beats a wrong-guess apply):

1. Exact line-sequence match (line tuples equal).
2. Whitespace-flexible match: ignore leading whitespace of each SEARCH line when locating.
3. Retry after dropping a spurious leading blank SEARCH line (models add them; Aider issue #25).
4. `...` elision handling: if SEARCH/REPLACE contain standalone `...` lines, split both on them;
   piece counts must pair and all `...` lines must be identical on both sides; then apply each
   piece pair by exact match. Any mismatch → fall through to 5.
5. **Cross-file retry:** if the named file fails, try the block against every file in the
   session read-set; a match applies there and is reported ("applied in <path> instead").
6. All strategies fail → structured failure feedback returned to the model as the next
   observation — this *is* the repair loop. Format (Aider's, verbatim semantics):

       # N SEARCH/REPLACE block(s) failed to match!
       ## SearchReplaceNoExactMatch: This SEARCH block failed to exactly match lines in {path}
       <<<<<<< SEARCH
       {original}=======
       {updated}>>>>>>> REPLACE
       Did you mean to match some of these actual lines from {path}?
       {best window with similarity ≥ 0.6, padded ± 5 lines}
       [if REPLACE text already present in file: "Are you sure you need this block? The
        REPLACE lines are already in {path}!"]
       The SEARCH section must exactly match an existing block of lines including all white
       space, comments, indentation, docstrings, etc
       [if other blocks applied: "The other N blocks were applied successfully. Don't re-send
        them. Just reply with fixed versions of the block(s) above that failed to match."]

- Applied blocks are journaled (path, before, after) to the undo stack **before** the file write.
- **Read-before-edit ledger:** a mutation requires its file to have been read this session
  (via `read` or `map_drill`). Violation → auto-inject the file (as a read), notify, and retry
  the block once. This replaces smallcode's compound `read_and_patch` tool with identical effect
  and less surface — **DECIDED**.

### 6.4 State Machine (`rusta-core/state.rs`) — phase gates (R5)

Four states (SmallCTL's six phases simplified: author/execute merged into `Editing`, repair
folded into `Verifying` feedback). **Canonical tool registry — 10 tools, DECIDED:**
`read`, `grep`, `glob`, `map_refresh`, `map_drill`, `dispatch`, `ask`, `edit`, `write`, `shell`.

| State | Available tools | Exit gate |
| ------- | ----------------- | ----------- |
| `Exploring` | read, grep, glob, map_refresh, map_drill, dispatch, ask | plan drafted |
| `Planning` | same as `Exploring` | **plan approval** (user y/n; `/auto` approves) |
| `Editing` | all 10 (shell approval-gated, §6.12) | edit batch applied |
| `Verifying` | read, grep, glob, map_refresh, map_drill, shell, ask | validation green → done; failed → back to `Editing` |

- Transitions fire only on scaffold events (`PlanApproved`, `EditsApplied`, `ValidationPassed`,
  `ValidationFailed`, `UserInterrupt`) — a closed `enum`; invalid transitions are impossible by
  construction (exhaustive match), never silently ignored (SmallCTL `PhaseContract`).
- A tool request unavailable in the current state → 1–2 line corrective note naming the current
  state and the transition that unlocks the tool (cheap context; small models learn the phase
  within one turn).
- `edit`/`write`/`shell` handlers are simply not registered in read-only states — `fs::write` is
  unreachable there (SmallCTL's "blocked_tools" enforced by construction, not by prompt).
- **DECIDED:** no 2-stage tool routing (smallcode) in v1 — ten one-line tool descriptions fit the
  <500-token core prompt (little-coder evidence). Two-stage selection is parking-lot (§14).

**Tool reference** (JSON input keys → behavior; results truncated per §6.1 caps):

| Tool | Input keys | Behavior |
| ------ | ------------ | ---------- |
| `read` | `path`, `from?`, `to?` | numbered file slice |
| `grep` | `pattern`, `glob?` | case-sensitive regex matches as `file:line: text` |
| `glob` | `pattern` | matching path list |
| `map_refresh` | — | re-render the repo map (§6.5) |
| `map_drill` | `path` + (`name` \| `from`/`to`) | definition span or line window; credits the ledger |
| `dispatch` | `task` or `tasks` | read-only sub-coders (§6.8) |
| `ask` | `question` | pauses the turn, surfaces the question to the user; the reply returns as the next observation and the turn continues; at most one pending `ask` per turn |
| `edit` | `path`, `search`, `replace` | tool-call form of a §6.3 block — identical apply chain |
| `write` | `path`, `content` | full-file write; existing file requires a prior ledger read |
| `shell` | `command` | §6.12 safety policy |

- **One edit mechanism, two syntaxes:** text SEARCH/REPLACE blocks (§6.3, canonical) and the
  `edit`/`write` tool calls both funnel into the *same* apply chain, undo journal, and
  read-before-edit ledger — never two implementations. In read-only states, either pathway yields
  the standard corrective note ("draft a plan first"), never a silent drop.
- **Read-only Q&A:** a request the model answers with prose + read-only tools, without drafting a
  plan, simply ends the turn; the state remains `Exploring`. `PlanApproved` fires only when the
  model drafts a plan for a change-task.

### 6.5 Repo Map (`rusta-repomap`) — AST context compression (R6)

Pipeline (port of Aider `repomap.py` semantics; the numbers are normative):

1. **Files:** git-tracked source files (`git ls-files`), filtered to configured languages;
   `.gitignore`d paths excluded. If not a git repo, recursive walk with built-in ignores
   (`target/`, `node_modules/`, `.git/`, `dist/`).
2. **Tags:** per file, a tree-sitter query (`.scm`, defs + refs — port from Aider's queries)
   yields `Tag { rel_fname, name, kind: def|ref, line }`. If a file's query yields defs but no
   refs, backfill refs from an identifier-token word scan so the file still connects to the graph.
3. **Graph:** one node per file. For each identifier defined in `D` files, referenced by a file
   `r` with `n_r` references: edge `r → definer` weight = `mul / (|D| · n_r)`, where `mul`:
   ×10 identifier explicitly mentioned by the user; ×10 snake/kebab/camelCase identifier with
   length ≥ 8; ×0.1 identifier starting with `_`; ×0.1 identifier defined in > 5 files.
   Identifiers defined but never referenced add a self-edge of weight 0.1 (keeps singletons
   rankable).
4. **Ranking:** personalized PageRank — damping 0.85, power iteration until Δ < 1e-6 or 100
   iterations (~40 lines of Rust; no graph crate). Personalization vector: `100/N` baseline per
   file; `+100/N` if the file is in the session chat-set or mentioned by the user; `+100/N` if
   any path component matches a user-mentioned identifier.
5. **Rendering:** files in rank order; session files excluded (their content is already in
   context). Per file: def lines as lines-of-interest with up to 8 surrounding context lines,
   header `path/to/file.rs:`; every rendered line truncated to 100 chars.
6. **Token fitting:** estimate map cost by tokenizing ≤ 100 evenly-spaced rendered lines and
   scaling by total/sampled characters (Aider's sampling trick — no full tokenization); while
   over budget, drop the **middle-ranked** files and re-render (top and bottom of the ranking are
   the most informative).
7. **Cache:** `(path, mtime, size, query_version) → Vec<Tag>` in-memory only for v1 (restart
   re-scans; native tree-sitter parses are ms-per-file).
8. **`map_drill` tool:** returns one definition's full span or a line window of a file — and
   credits the read-before-edit ledger (§6.3).

### 6.6 Context Manager (`rusta-core/context.rs`) — purity + JIT + compression (R7)

**Core prompt < 500 tokens** (CI invariant): persona (1 line) · current state + exit gate (2) ·
10 tool one-liners + the ```tool call syntax example (~150) · SEARCH/REPLACE example (~80) ·
output rules (~80) · git footer (~20). Everything else is injected JIT or not at all.

**JIT skill cards** (`skills/*.md`, markdown + YAML front-matter — little-coder's schema):
`name`, `type: tool|knowledge|recovery`, `triggers: [tool names, error kinds, keywords]`,
`priority: 1–9`, `token_cost` (declared; CI verifies ≤ 120), `user-invocable: bool`; body is a
short imperative card. Injection: on trigger match, at most **2** cards appended as a trailing
system note; evicted at task end. Never resident by default.

**History compression:** when the assembled prompt would exceed 60% of the context window,
summarize the oldest turns into a single `assistant` summary message ("episodic memory"), keeping
verbatim: the last 3 turns, every applied edit block, and every error-feedback exchange.

**Loop mitigation (FAMA-lite)** — concrete port of SmallCTL `fama/` mechanics:

- Detectors: (a) stagnation counter per `tool|args-fingerprint`, trips at ≥ 3 identical calls;
  (b) ≥ 3 consecutive identical tool fingerprints in history; (c) ≥ 2 identical validator outputs
  in a row; (d) no-op edit (REPLACE == SEARCH).
- Response: inject a **mitigation capsule** — a single imperative line naming the exact next
  action. Seed capsule texts (adapted from SmallCTL's proven set):
  - `MUTATION REQUIRED: You have read enough. Emit ONE edit block this turn, then verify.`
  - `Do not repeat the same tool call unchanged; use prior output or switch to a different action.`
  - `Use the evidence already in context before reading or running anything again.`
- Budget: ≤ 180 tokens total, max 5 active, deduplicated, expire after 3 turns. Escalation at
  2× threshold: automatic state regression (`Editing → Planning`) + user notification.

### 6.7 Validation (`rusta-validate`) — Reflexion gate (R8)

Configured per project (`[validate] commands = ["cargo check --workspace", "cargo clippy --workspace -- -D warnings", "cargo test --workspace"]`).
After each applied edit batch, run validators; format output for the model (deduped, first error

- last error, ≤ 30 lines) and feed it back as an observation *before* the user sees results —
the model gets first repair attempts (bounded: 3; then surface to the user). Exit gate: `Verifying`
passes only when all validators are green. Zero-test detection: "0 tests" in test output →
capsule prompting real tests (SmallCTL lesson). Compile errors map back to file:line — the model
receives clickable locations.

### 6.8 Sub-Coder Dispatch (`rusta-dispatch`) — actor isolation (R9)

The `dispatch` tool spawns isolated read-only sub-coders (little-coder semantics, verified from
its `skills/tools/dispatch.md`): input is either a single `task` string or `tasks` — an array of
`{label, task}`, **max 4**, each with a distinct label. Sub-coder toolset = `{read, grep, glob,
map_refresh, map_drill}` — read-only, **no shell in v1 (DECIDED)**. Each sub-coder is a fresh
context (core prompt + task) with its own turn cap (6); its final message is the report,
**≤ 400 tokens** (enforced by its wrap-up prompt); the full sub-transcript goes to the session
log only — never main context. Reports return labeled: `SUB-CODER “label” REPORT:` followed by
the report. **Embedded-backend constraint:** one loaded model, one inference thread → sub-coder
requests serialize (correctness preserved; true parallelism exists only on the HTTP backend) —
documented in README; KV-cache snapshots for cheap sub-contexts are parking-lot (§14).

### 6.9 CLI (`rusta-cli`) — Aider-style REPL (R3)

`reedline` REPL. `/add <glob>` adds files to the session set (chat-set), `/drop`, `/undo` (revert
last edit batch via journal), `/diff`, `/map` (show repo-map), `/state` (state + tool matrix),
`/auto` (auto-approve plan gate + shell), `/model`, `/backend`, `/skills <name>` (user-invocable
cards), `/resume <file>`, `/save`, `/exit`. Git: auto-commit after each applied edit batch with
message `rusta: <one-line summary of the user request>`; `/undo` reverts the commit via the
journal (never `git reset` on unrelated commits). Non-interactive: `rusta -c "prompt"` runs one
agent turn and exits (shell tool denied by default in this mode, §6.12).

### 6.10 Session Persistence Schema (`rusta-core/session.rs`)

`~/.rusta/sessions/<slug>-<UTC date>.jsonl`, append-only, one JSON object per line. Event types:
`SessionStart { config, backend }` · `UserMessage` · `AssistantMessage { content }` ·
`ToolCall { name, input }` · `ToolResult { status, summary, truncated }` ·
`EditApplied { path, before_hash, after_hash }` · `ValidationRun { command, exit, summary }` ·
`StateChange { from, to, reason }` · `Summary { covers_turns, text }` · `Commit { sha, message }` ·
`Dispatch { label, report }` · `SessionEnd`. Full before/after text of edits lives in a sidecar
`<slug>.diffs.jsonl` (keyed by hash) so the chat log stays small. `/resume <file>` replays events
to reconstruct messages, ledger, undo stack, and state machine.

### 6.11 Error Taxonomy

Each crate defines a `thiserror` enum (`rusta_llm::Error`, `rusta_edit::Error`, …) with variants
the *model* can act on: `SearchNoMatch { path, hint }`, `FileNotRead { path }`,
`ToolBlocked { tool, state }`, `Io`, `Backend { .. }`. `anyhow` appears only at the CLI boundary.
Rule: every error surfaced to the model MUST contain an actionable remedy; underlying errors are
wrapped, truncated to 10 lines, with a capsule appended when a detector matches (§6.6).

### 6.12 Shell & Safety Policy

The `shell` tool runs only in `Editing`/`Verifying`, only after interactive approval (y / n /
`a` = always this session; `/auto` implies `a`; non-interactive `-c` mode denies by default).
Deny-list (regexes, config-extendable): `rm -rf /`, `sudo`, `git push --force`, `dd`, `mkfs`,
fork bombs, `curl … | sh`, `shutdown`, `reboot`, and any write outside the repo root. Execution:
cwd = repo root; timeout 60 s (config); output truncated per §6.1; no PTY — interactive commands
are detected and denied with the remedy "pass flags for non-interactive mode"; minimal
environment (PATH, HOME, LANG + config allow-list).

## 7. Configuration (`rusta.toml`, discovered in cwd → parents → `~/.rusta/`)

```toml
[backend]
kind = "http"            # "http" | "embedded"
base_url = "http://127.0.0.1:8080/v1"   # full API root incl. /v1 (§6.2)
api_key_env = "RUSTA_API_KEY"           # optional; read from env, never stored

[backend.embedded]        # used when kind = "embedded"
model_path = "~/models/qwen3-coder-30b-a3b-q4_k_m.gguf"
ctx_size = 32768
gpu_layers = 999          # 0 = CPU
strict_grammar = false    # opt-in GBNF (§6.2)

[model]
name = "qwen3-coder-30b-a3b"
context_window = 32768    # HTTP backend; embedded reads GGUF metadata
max_tokens = 4096
temperature = 0.2

[agent]
max_turns = 16
auto_approve = false      # plan gate + shell

[repomap]
max_tokens = 1024

[validate]
commands = ["cargo check --workspace", "cargo clippy --workspace -- -D warnings", "cargo test --workspace"]

[shell]
timeout_secs = 60
allow = []                # extra allow-listed command prefixes
deny = []                 # extra deny regexes (§6.12)
```

## 8. Milestones (build in order; each merges only with all acceptance criteria green)

Legend: ✅ — completed; all acceptance criteria verified locally (fmt, clippy `-D warnings`, tests, LoC gate).

| # | Deliverable | Acceptance criteria (must all pass) |
| --- | ------------- | ------------------------------------- |
| M0 ✅ | Workspace skeleton: 8 crates, CI (fmt + `clippy -D warnings` + test + LoC gate), `rusta --version` | builds; `scripts/loc_budget.sh` wired into CI; deny.toml; LICENSE MIT — **verified 2026-09-10** |
| M1 ✅ | `Backend` trait + `HttpBackend` (SSE streaming, retries, config precedence, base_url error hints, `tool_calls` passthrough) + mock SSE server test | token stream from mock server e2e; retry/backoff tested with a flaky mock; 404-hint tested; JSONL session scaffold (`§6.10` events round-trip) — **verified 2026-09-10** (11 unit + 9 e2e + 3 session tests; enum dispatch per §6.2) |
| M1.5 ✅ | `EmbeddedBackend` (feature `embedded`): GGUF load, chat template, inference thread → mpsc, exact token counting, cancellation flag | `#[ignore]` GGUF e2e (env `RUSTA_TEST_GGUF`) — run live against a real GGUF (3/3: load+exact counts, `complete`, streamed deltas+finish); template fallback/conversion unit tests + stop-emitter corpus; default build compiles without cmake (llama.cpp never enters the default graph) — **verified 2026-09-10** (clippy `-D warnings` clean with *and* without the feature; `llama-cpp-2 =0.1.156` pinned; single serialized inference thread; temp→top_p→greedy chain; ChatML fallback; batch-position-aware sampling fix) |
| M2 | `rusta-edit`: parser + apply chain + failure feedback + ledger | Aider fixture corpus + malformed-input corpus green; `...`-elision tests; cross-file retry tests; failure-feedback snapshot tests; ledger auto-inject test; property test: parser never panics on arbitrary input |
| M3 | `rusta-core`: state machine + session persistence + prompt compiler | state-transition table exhaustively tested; core prompt < 500 tokens invariant; `/resume` reconstructs ledger + state from a recorded session |
| M4 | `rusta-repomap`: extraction, ranking, budget-fitted rendering, cache, `map_drill` | golden snapshot maps on 3 sample repos; token-fitting never exceeds budget; cache hit path tested with mtime bumps |
| M5 | `rusta-core/context.rs`: JIT cards, compression, FAMA-lite | trigger-matrix test; card ≤ 120 tokens invariant; summarization keeps edit blocks verbatim; each loop detector trips + correct capsule |
| M6 | `rusta-validate` + wiring into the Verifying exit gate | feedback format ≤ 30 lines; 3-repair-bound then surface; zero-test capsule |
| M7 | `rusta-dispatch` + `rusta-tools` registry | 4 parallel sub-coders on mock backend, labeled reports ≤ 400 tokens; embedded serialization test (mock); tool-state matrix test |
| M8 | `rusta-cli`: REPL, `/commands`, git auto-commit/undo, config | e2e scripted sessions on the mock server: edit loop → validate → repair → commit; `/undo` restores file + reverts commit |

## 9. Testing Strategy

- **Parser corpus** (`tests/corpus/`): golden SEARCH/REPLACE fixtures (clean, fenced, missing
  markers, chained-DIVIDER, `...` elisions, DeepSeek-style fenced filenames, duplicate matches,
  empty-SEARCH new files) each with expected parse **and** expected apply result; malformed-input
  corpus asserting degradation to prose + corrective note.
- **Tool-call corpus**: fenced single, JSON array form, native `tool_calls` passthrough, unknown
  name, malformed JSON → corrective note.
- **HTTP e2e** uses a hand-rolled mock SSE server (`tests/mock_server.rs`, tokio + std TcpListener
  - manual SSE chunks) — keeps the dependency tree lean vs wiremock.
- **Embedded tests** are `#[cfg(feature = "embedded")]` + `#[ignore]` (require a real GGUF path via
  `RUSTA_TEST_GGUF`), so CI default runs never need cmake.
- **Invariant tests**: core-prompt < 500 tokens; skill cards ≤ 120 tokens; forbidden-unsafe lint;
  LoC budget gate (§12).

## 10. Dependencies (every entry must earn its place; minimal feature sets)

- `tokio` (rt-multi-thread, macros, sync, fs, process) · `reqwest` 0.12 (rustls-tls, json, stream;
  default-features off) · `serde`/`serde_json`/`toml` (config & tool-call args only — never the
  edit format) · `clap` 4 (derive) · `anyhow` + `thiserror` · `reedline` (REPL) · `futures` (streams)
- `tree-sitter` + per-language crates (rust, python, javascript/typescript, go, c, cpp), pinned.
- `llama-cpp-2 =0.1.156` — **optional**, feature `embedded` (+ `sampler`); `embedded-cuda` adds
  CUDA. Pinned exact: the crate intentionally does not follow semver. Companion: `encoding_rs 0.8`
  (optional, same feature) — llama-cpp-2's detokenization API takes an `encoding_rs::Decoder` by
  type, so incremental UTF-8-safe token decoding requires the direct dependency.
- Dev-only: `insta`, `tempfile`.
- Explicitly avoided: async-trait (native async traits / enum dispatch), networkx-style graph
  crates (PageRank is ~40 lines), axum/actix, ORM/DAL, anything pulling OpenSSL.

## 11. Risks & Mitigations

| Risk | Mitigation |
| ------ | ------------ |
| `llama-cpp-2` API churn (no semver) | Pin exact version; all usage isolated behind `rusta-llm`'s `Backend` trait; upgrade deliberately with contract tests |
| tree-sitter grammar drift | Pin grammar crates; golden snapshot tests catch regressions |
| GPU build complexity (cmake/CUDA) | Default artifact HTTP-only; `rusta-full`/`embedded-cuda` documented as opt-in; CI matrix builds both |
| Scope creep breaking R1 | LoC budget CI gate (§12) + scope fences (§4); new features go to opt-in crates |
| Small-model format variance | Canonical fenced-JSON tool calls (proven by little-coder with 9B models) + native `tool_calls` passthrough + conservative drop-with-corrective-note (smallcode pattern) + recovery parser + (embedded) opt-in GBNF |
| Apply-chain edge cases | Port Aider's proven algorithm semantics *exactly* (§6.3), incl. disabled-edit-distance decision; fixtures cover duplicate matches, elisions, cross-file retry |
| Small models derailed by agent loops | FAMA-lite detectors + capsules (§6.6), turn caps, state regression escalation |

## 12. R1 Enforcement — `scripts/loc_budget.sh` (CI gate)

```bash
#!/usr/bin/env bash
set -euo pipefail
PROD=$(find crates -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' -type f -exec cat {} + | wc -l)
TESTS=$(find crates \( -path '*/tests/*' -o -name 'tests.rs' \) -name '*.rs' -type f -exec cat {} + | wc -l)
TOTAL=$((PROD + TESTS))
echo "production=${PROD} tests=${TESTS} total=${TOTAL}"
[ "$PROD" -le 15000 ] || { echo "FAIL: production budget exceeded (>$PROD)"; exit 1; }
[ "$TOTAL" -le 20000 ] || { echo "FAIL: total budget exceeded (>$TOTAL)"; exit 1; }
```

Canonical counting is this script (`wc -l`); `tokei` is an optional cross-check. Data files
(`.scm` queries, `skills/*.md`, prompt fragments) are reported separately, never counted.

## 13. Definition of Done — v1

- All milestones merged; CI green including the LoC budget gate and `clippy -D warnings`.
- Artifacts `rusta` (HTTP) and `rusta-full` (embedded) build for Linux and macOS; smoke-tested.
- README quickstart for both backends, incl. the spec's hardware/model guidance: 24GB VRAM →
  Qwen3-Coder 30B-A3B / Qwen3.6 27B MTP / Gemma 4 31B IT QAT; 12–16GB → gpt-oss-20b /
  DeepSeek-R1 14B / Gemma 4 12B; ≤8GB → Qwen3.5 9B MTP; CPU-only → 4-bit MoE A3B models via
  llama.cpp with `-t` = physical cores.
- E2E scenarios pass on the mock server; GGUF manual test matrix documented.
- Verified: ≤15k production / ≤20k total lines of Rust; §15 traceability matrix fully checked.

## 14. Parking Lot (post-v1, separate opt-in crates — never core bloat)

TUI (ratatui) · GBNF strict grammar as default · KV-cache snapshots for cheap sub-coder contexts ·
out-of-tree Aider-Polyglot-style benchmark harness · MCP client support · Observer-style
multi-agent teams beyond dispatch · per-phase model routing (plan on strong, execute on cheap) ·
2-stage tool routing (smallcode) · per-model tool-call format adapters (smallcode
liquid-parser pattern) · udiff edit format (Aider `udiff_coder`) as an alternative model dialect.

## 15. Traceability Matrix (audit at M8 — every row must point to a shipped, tested subsystem)

| Req | Subsystem (section) | Milestone |
| ----- | -------------------- | ----------- |
| R1 | §4 budget, §12 gate | M0 (wired), continuously |
| R2 | §6.2 backends | M1, M1.5 |
| R3 | §6.9 CLI | M8 |
| R4 | §6.3 edit protocol | M2 |
| R5 | §6.4 state machine | M3, M7 |
| R6 | §6.5 repo map | M4 |
| R7 | §6.6 context manager | M3, M5 |
| R8 | §6.7 validation | M6 |
| R9 | §6.8 dispatch | M7 |
| R10 | §2, workspace lints | M0 |
| R11 | §6.2 (offline HTTP default; embedded fully local), §10 (no telemetry deps) | all |
