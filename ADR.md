# Rusta — Architecture Decision Record

**Project:** Rusta — a lean AI coding-agent harness for small, locally hosted coding LLMs (8B–35B parameters).
**ADR version:** 1.0 · **Date:** 2026-09-15 · **Status:** Accepted — v1 milestone-complete (M0–M8), six audit rounds remediated, **pre-alpha** (see §17).

**Supersedes** the former `DEVELOPMENT_PLAN.md` v1.2 (the normative build specification, §0–§15
plus its 2026-09-13/14/15 errata) and `AUDIT_REPORT.md` (the consolidated findings of six review
rounds). Both are folded into this document and deleted; this is now the single source of truth.

## The numbering contract

Roughly 650 doc comments, test names and CI steps across the eight crates cite this
specification by section — `§6.3 rule 4`, `§6.1 caps`, `ADR §12`. **Section numbers §0–§15 are
therefore load-bearing and are preserved here exactly as they were.** A citation of the form
`ADR.md §6.5 step 3` resolves against this document and means what it meant before. Do not
renumber these sections; append new material as §16 and beyond.

Two things changed in the fold-in:

- The plan's dated **errata** — corrections issued after v1.1 shipped — are inlined as
  **Revision** notes at the point they apply. They are kept rather than silently absorbed
  because each records *why* the code diverges from the obvious reading, which is the part an
  ADR exists to preserve.
- The audit report's 25 findings are condensed into **§16**, as a decision record: what was
  wrong, what was decided, and the three standing acceptance rules that came out of it.

---

## 0. How to Use This Document (implementation contract)

This is the single source of truth for Rusta. It is written to be executed by an LLM-driven or
human developer **without access to any other context**.

1. Sections §6.1–§6.12 are **normative specifications**. Where a subsystem cites a reference
   implementation in §3, read that file first; borrow its *semantics*, not its code or host
   language.
2. When this document is silent, defer to the cited reference implementation's behaviour. When
   this document conflicts with a reference, **this document wins**.
3. Items marked **DECIDED** are final — do not re-litigate them.
4. Do not add dependencies beyond §10, tools beyond the §6.4 registry, or crates beyond §5.
5. R1 (line budget) is contractual: production ≤ 15,000 lines, total ≤ 25,000, counted by the
   §12 script. Tests are part of the deliverable and count against the total.
6. Feature completeness is audited against the §15 traceability matrix; §13 is the Definition
   of Done.
7. Rust snippets here are **illustrative sketches, not compile targets** — adapt signatures and
   paths to the actual crates, but preserve the stated semantics exactly.
8. §16's acceptance rules apply to every change: a detector must prove it is looking; a change
   to what a data structure contains must be checked against every consumer; "the guard says
   so" is not evidence.

## 1. Mission & Design Philosophy

Small local models are not bad at coding — they are bad at managing scaffolds built for frontier
models. Standard harnesses fail on 8B–35B models through context exhaustion, JSON hallucination,
and task-loop derailment. Evidence motivating the project: swapping *only the scaffold* lifted a
9B Qwen from **19.1% → 45.6%** on Aider Polyglot; SmallCode reports **87%** internal success on
a 4B-active model.

Rusta combines five proven scaffold pillars into one Rust/Tokio binary:

1. **Forgiving mutations** (Aider + smallcode) — plain-text SEARCH/REPLACE for edits, never
   JSON; a recovery parser that survives missing fences and stray markdown.
2. **Context compression** (Aider + SmallCTL) — tree-sitter AST repo map instead of raw file
   reads; episodic summarization of past turns.
3. **State-machine guardrails** (SmallCTL) — a strict Explore → Plan → Write lifecycle; the tool
   registry filters itself by state; mutation tools are physically unreachable in read-only
   phases.
4. **Actor/dispatch** (little-coder + Observer) — isolated Tokio sub-coders for research; only
   summarized answers re-enter the main context.
5. **Context purity & JIT prompts** (little-coder) — core system prompt < 500 tokens;
   just-in-time skill-card injection triggered by intent and errors.

## 2. Hard Requirements (v1)

| # | Requirement | Notes |
| --- | --- | --- |
| R1 | **≤ 15,000 lines of production Rust; ≤ 25,000 total including tests** (total raised from 20,000 on 2026-09-15 — see §12) | Counted on `*.rs` by the §12 script; enforced as a CI gate. Query files (`.scm`), skill cards (`.md`) and prompt templates are *data*, tracked separately. |
| R2 | **Dual LLM backend, user-selectable at runtime** | (a) OpenAI-compatible HTTP client — always compiled, the default; (b) **embedded llama.cpp** via `llama-cpp-2` behind the `embedded` cargo feature. Selected by `rusta.toml` `[backend] kind` or `--backend`. |
| R3 | CLI-first: Aider-style REPL | TUI / IDE extensions are out of scope for v1 (§14). |
| R4 | Plain-text edit protocol with a forgiving parser and Aider's proven apply chain | §6.3. |
| R5 | Phase-gated state machine + read-before-edit enforcement | §6.4. |
| R6 | Tree-sitter repo map, token-budget fitted, incrementally cached | §6.5. |
| R7 | < 500-token core prompt; JIT skill cards; history compression; loop-mitigation capsules | §6.6. |
| R8 | Instant auto-validation: compiler/linter feedback fed back to the model before the user sees results | §6.7. |
| R9 | Sub-coder dispatch over Tokio (isolated contexts, summarized returns) | §6.8. |
| R10 | `#![forbid(unsafe_code)]` in all core crates; the only permitted `unsafe` lives behind `llama-cpp-2` in the embedded backend | Enforced workspace-wide. |
| R11 | Works offline: no telemetry, no accounts, no cloud calls unless the user configures an HTTP endpoint | The only network path is the user-configured backend (§7). |

## 3. Reference Implementations

Studied at source during design and re-verified at each audit. Located at
`/home/vladimir/develop/refs/`. **Port the essence, not the bulk.**

| Repo | Lang | What Rusta extracts | Key files |
| --- | --- | --- | --- |
| **Aider** | Python | SEARCH/REPLACE format, apply fallback chain, failure-feedback contract; repo-map ranking + tree-sitter queries; git auto-commit UX | `coders/editblock_coder.py` (657 L), `repomap.py` (867 L), `queries/*.scm` |
| **little-coder** | TypeScript | JIT skill-card architecture and card schema; **canonical tool-call format** (fenced `tool` blocks); sub-coder dispatch semantics; strict read-before-edit rule | `skills/tools/*.md`, `skills/knowledge/*.md` |
| **Observer** | TS + Rust | Actor-model orchestration of ephemeral specialized agents; Rust CLI conventions | `cli/src/{runner,preflight,config,notify}.rs` |
| **smallcode** | JavaScript | Conservative drop-on-malformed parsing policy; compound-tool rationale | `src/tools/liquid_tool_parser.js` (314 L) |
| **SmallCTL** | Python | Phase contracts with per-phase blocked tools; write-session FSM; FAMA loop detection + mitigation capsules; Reflexion gate | `phases.py`, `guards.py`, `write_session_fsm.py`, `fama/*` |

**Why R1 is achievable:** the load-bearing mechanisms are *tiny* — Aider's entire edit engine is
657 lines, its repo map 867; smallcode's parser 314; SmallCTL's whole phase/guard/FSM core ~1.1k.
The five-figure totals come from web UIs, eval harnesses, TUI/LSP/RAG subsystems and
multi-provider matrices — all of which Rusta excludes.

**Findings from direct source reading that shaped the design:** (a) Aider's apply chain
*deliberately disables* edit-distance matching, failing with rich feedback instead — §6.3 adopts
this; (b) smallcode's parser is a per-model adapter with a conservative drop-on-failure policy —
it informed §6.1's malformed-input policy; (c) SmallCTL's capsules are one-line imperatives with
explicit next actions, budget-capped — §6.6 ports the mechanics.

## 4. Budget & Scope Fences

Per-crate design targets (indicative; only the §12 global totals are gated):

| Crate | Responsibility | Prod LoC | Test LoC |
| --- | --- | ---: | ---: |
| `rusta-llm` | backends, HTTP SSE client, embedded llama.cpp, token estimation | 2,000 | 800 |
| `rusta-edit` | forgiving parser, apply chain, failure feedback, ledger | 1,200 | 1,500 |
| `rusta-core` | state machine, session persistence, context manager, prompt compiler | 2,000 | 600 |
| `rusta-tools` | phase-gated registry, the 10 canonical tools (§6.4) | 1,600 | 500 |
| `rusta-repomap` | tree-sitter extraction, ranking, budget-fitted rendering, cache | 1,000 | 300 |
| `rusta-dispatch` | sub-coder actors, parallel read-only DAG | 600 | 200 |
| `rusta-validate` | config-driven validators, feedback formatting | 400 | 150 |
| `rusta-cli` | REPL, slash commands, render, git, config | 1,800 | 400 |
| glue | shared error types, small utils | 400 | 100 |
| **Total** | | **11,000** | **4,550** |

**Scope fences (NON-goals for v1):** no TUI, no IDE plugin, no LSP integration, no RAG/embedding
store, no MCP, no multi-cloud provider matrix, no in-tree benchmark suite, no plugin system, no
web UI, no remote/SSH orchestration, no fine-tuning tooling. Anything here that later proves
essential goes into a *separate* opt-in crate — never the core (§0 rule 4).

## 5. Workspace Layout

```text
rusta/
├── ADR.md                   ← this document (single source of truth)
├── Cargo.toml               ← workspace; features: embedded, embedded-cuda (opt-in)
├── rusta.toml.example       ← both backend configs documented
├── scripts/loc_budget.sh    ← R1 enforcement gate (§12)
├── crates/
│   ├── rusta-llm/           ← http.rs + embedded.rs (cfg-gated) + tokens.rs + types.rs
│   ├── rusta-edit/          ← parser.rs, apply.rs, ledger.rs (read-before-edit)
│   ├── rusta-core/          ← state.rs, session.rs, context.rs, prompt.rs, proc.rs
│   ├── rusta-tools/         ← registry.rs + the 10 tools (§6.4)
│   ├── rusta-repomap/       ← extract/rank/render/cache + queries/*.scm (data)
│   ├── rusta-dispatch/      ← actor.rs, dag.rs
│   ├── rusta-validate/      ← validators.rs
│   └── rusta-cli/           ← main.rs, repl.rs, agent.rs, commands.rs, render.rs, git.rs, config.rs
├── skills/                  ← JIT skill cards as markdown (data, versioned, LoC-exempt)
└── crates/*/tests/          ← e2e: mock-server fixtures, parser corpora, property tests
```

`proc.rs` hosts `capped_read`, shared by both subprocess call sites so §6.1's output caps bound
*memory*, not just the rendered result.

## 6. Normative Design — Subsystems

### 6.1 The Agent Loop, Tool-Call Wire Format & Turn Lifecycle

Rusta does **not** use the OpenAI `tools` request parameter. The model emits plain text
containing fenced tool calls and/or SEARCH/REPLACE edit blocks; Rusta parses, executes, and
re-prompts.

**Canonical tool-call format** (little-coder lineage — proven with 9B models; JSON is allowed
*only* here, as trivial two-key objects — **never for edits**):

````text
```tool
{"name": "read", "input": {"path": "src/main.rs"}}
```
````

Accepted forms, in priority order: (1) canonical fenced ` ```tool ` blocks, one call per block;
(2) fenced blocks containing a JSON **array** of calls; (3) native OpenAI `tool_calls`
passthrough for servers that emit it. Unknown tool `name` → corrective note listing the valid
tools for the current state. Unparseable block → prose plus a corrective note (smallcode's
conservative drop-on-failure policy). **DECIDED:** Rusta never sends `tools`/`functions`
parameters — the syntax cheat-sheet lives in the < 500-token core prompt (context purity).

**Turn lifecycle** (one turn = one model completion):

1. Stream the completion; parse tool calls and edit blocks incrementally (each item emitted as
   soon as its closing marker arrives).
2. If the completion contains ≥ 1 actionable item → execute in **document order** (edits via
   §6.3, tools via §6.4) → append observations → start the next turn.
3. A completion with no tool calls and no edit blocks ends the agent turn; its content is the
   answer shown to the user.
4. Hard cap: 16 model turns per user request (configurable). On cap, inject a wrap-up capsule
   and return control to the user.
5. Ctrl-C aborts the stream, discards the partial turn, preserves session state.

**Observation contract** — tool results are appended as `user`-role observation blocks:

```text
TOOL RESULT read (ok)
<content, truncated>
```

Hard truncation caps: read 2,000 lines / 64 KiB; grep 200 matches (and 200 chars per line);
glob 1,000 entries; shell 16 KiB combined stdout+stderr; **dispatch 400 tokens per sub-coder
report** (§6.8 — a four-task fan-out returns up to four of them plus labels), with each
sub-coder observation capped at 1,500 tokens; repo map as rendered (§6.5). On truncation append
`… [truncated N lines/bytes]` — and only when a cap actually truncated, never when the caller
asked for a bounded window. A cap's own marker is charged against the cap, never added on top.
Errors are returned as actionable text (§6.11), never as panics or stack traces.

> **Revision (2026-09-15).** §6.1 previously listed "dispatch 400 tokens" among per-*result*
> caps while §6.8 said 400 per *report* — readings that differ 4× on a four-task fan-out.
> Resolved in §6.8's favour and stated explicitly above.

Caps must bound **memory**, not merely output: subprocess pipes drain incrementally through
`rusta_core::proc::capped_read`, and `read` streams line-wise rather than loading whole files.

### 6.2 Backends (`rusta-llm`) — dual, runtime-selectable (R2)

```rust
// enum dispatch — a closed set, no trait objects, no async-trait (§10)
pub enum Backend { Http(HttpBackend), #[cfg(feature = "embedded")] Embedded(EmbeddedBackend) }
impl Backend {
    pub async fn stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>>;
    pub async fn complete(&self, req: ChatRequest) -> Result<String>;   // non-streaming
    pub fn count_tokens(&self, text: &str) -> u64;
    pub fn context_window(&self) -> u64;
    pub fn kind(&self) -> BackendKind;   // lets dispatch schedule without matching internals
    pub fn stop(&self);                  // cancellation; no-op on HTTP (drop the receiver)
}
```

**`HttpBackend`** — always compiled, default. `POST {base_url}/chat/completions` with
`stream: true`; SSE parsing via `reqwest` + `futures::StreamExt`; incremental `delta.content`,
finish reason, and native `tool_calls` deltas assembled into complete calls. Retries: 3
attempts, exponential backoff (250 ms → 2 s), connection errors and 5xx only. Context window
comes from `[model] context_window`, default 32768 — all budgeting uses this number.
**`base_url` must be the full API root including `/v1`**; a 404 on `/chat/completions` produces
an error hint listing correct forms for common servers. `complete()` uses `stream: false`.
Wire-supplied `tool_calls` indices are **bounded** (≤ 64) and rejected as `Error::Malformed`
beyond that — an unbounded `resize` on a `u32` index is an abort, not a recoverable error, and
§6.1's never-panic posture forbids it.

**Token estimation:** no tokenizer dependency — `tokens.rs` heuristic `ceil(chars / 3)`,
deliberately conservative. Acceptable because every budget threshold (§6.5 fitting, §6.6
compression trigger, §6.8 report caps) carries headroom; it is the single swap-in point for a
real tokenizer.

**`EmbeddedBackend`** (feature `embedded`) — loads a GGUF via `llama-cpp-2 =0.1.156` (pinned
exact: the crate ignores semver), applies the model's chat template with a ChatML fallback,
samples with `LlamaSampler` (temp 0.2, top_p 0.9 defaults), and runs inference on a dedicated OS
thread streaming tokens over `tokio::sync::mpsc` (the C API blocks; the async boundary stays
clean). Context window and exact token counts come from the loaded model. Cancellation: a
shutdown flag checked between tokens, driven by `Backend::stop()`.

> **Revision (2026-09-13).** v1.1 specified an optional strict GBNF grammar
> (`strict_grammar = true`). It was never implemented: the key parsed, was documented, and did
> nothing. Rather than ship a silent no-op the key was **removed**; GBNF stays in §14. This is
> the archetype of a defect class this project keeps finding — a specified, documented mechanism
> no production path reaches (see A5 in §16).

**Both backends** consume the same `ChatRequest { messages, max_tokens, stop, temperature }`;
the choice is invisible to every layer above.

### 6.3 Edit Protocol (`rusta-edit`) — "just text" mutations (R4)

Canonical block (Aider format — small models know it from training corpora):

```text
path/to/file.rs
<<<<<<< SEARCH
exact existing lines
=======
replacement lines
>>>>>>> REPLACE
```

**Parser** (single pass; HEAD/DIVIDER/UPDATED markers matched on `.trim()`ed lines):

1. `<<<<<<< SEARCH` (HEAD) … `=======` (DIVIDER) … `>>>>>>> REPLACE` (UPDATED). DIVIDER ends
   SEARCH collection; UPDATED **or a new DIVIDER** ends REPLACE collection (the latter
   immediately starts the next block — models chain blocks without UPDATED).
2. **New file:** HEAD immediately followed by DIVIDER (empty SEARCH) → create-file edit. A full
   rewrite of an existing file uses the `write` tool (§6.4), never an edit block.
3. **Filename resolution**, in order: scan up to 3 lines above HEAD, skipping fences
   (DeepSeek-style fenced filenames accepted) → candidates matched against the session file-set:
   exact path → basename → fuzzy (similarity ≥ 0.8) → any candidate containing a dot. If no
   candidate and a previous block in the same response named a file → continuation. Otherwise
   fail with the missing-filename corrective error. Marker lines can never hijack resolution.
4. Forgiveness: stray markdown fences around HEAD/UPDATED stripped; CRLF normalized; a missing
   final UPDATED still commits the block — **after stripping an UPDATED marker the model ran
   onto the end of the last REPLACE line**, which is the usual reason the marker looks missing.
   The strip runs on **every** commit out of the REPLACE state, not only at end-of-stream.
5. A fenced ` ```bash/sh/shell ` block that is **not** part of an edit is surfaced as a
   *suggested command* requiring confirmation — never auto-executed.
6. The parser never panics; malformed input degrades to prose + corrective note.

> **Revision (2026-09-13).** Aider rejects the missing-UPDATED case outright. Forgiving it
> without the strip wrote `>>>>>>> REPLACE` into the user's source and auto-committed it.
>
> **Revision (2026-09-15).** The strip was applied only in the end-of-stream branch, so a block
> closed *mid-stream* — by a following UPDATED, or a chained DIVIDER — still committed the
> marker fused to its last content line, reported success, and emitted no note. Any completion
> with more than one edit block hit it (A1, §16).
>
> **On CRLF:** `"x\r"` followed by CRLF normalizes to `"x\r\n"`, because a content CR
> immediately before a line break is textually indistinguishable from a CRLF ending. A blanket
> "no CR survives in block text" assertion is therefore *not available* and must not be
> reintroduced; the property tests assert parse determinism instead.

**Apply chain** (per block, strictly in order — a port of Aider's proven sequence. **DECIDED:**
no edit-distance matching; for small models a clear retry request beats a wrong-guess apply):

1. Exact line-sequence match.
2. Whitespace-flexible match: ignore each SEARCH line's leading whitespace when locating.
3. Retry after dropping a spurious leading blank SEARCH line (models add them; Aider issue #25).
4. `...` elision handling: **only when SEARCH contains standalone `...` lines** — split both
   sides on them; piece counts must pair and all `...` lines must be identical on both sides;
   then apply each piece pair by exact match. Any mismatch → fall through to 5.

   > **Revision (2026-09-13).** Aider bails out when there are no `...` pieces
   > (`if len(part_pieces) == 1: return`). Without that guard the single-piece path degenerates
   > into a raw substring replace ignoring line boundaries — exactly the wrong-guess apply this
   > section's DECIDED clause rules out.
5. **Cross-file retry:** if the named file fails, try the block against every file in the
   session read-set; a match applies there and is reported ("applied in `<path>` instead").
6. All strategies fail → structured failure feedback as the next observation — this *is* the
   repair loop. Format (Aider's, verbatim semantics): the failing block echoed, `Did you mean to
   match some of these actual lines from {path}?` with the best window (similarity ≥ 0.6, padded
   ± 5 lines), a note if the REPLACE text is already present, the exact-match requirement, and a
   don't-re-send note listing how many other blocks applied.

**Confinement, journaling, ledger:**

- **Repo-root confinement:** a resolved path that is absolute or contains `..` is refused with a
  corrective note, never written. This fence applies to **both** edit syntaxes (§6.4), not only
  the tool-call forms.

  > **Revision (2026-09-13).** v1.1 stated the rule only for `shell` (§6.12). The implementation
  > fenced the tool pathway but not the canonical text pathway, so an absolute or `../` filename
  > above a SEARCH block wrote outside the workspace unchecked.

- **One mutation funnel.** Every file mutation in the crate passes through a single `guarded()`
  entry point that checks confinement before touching the filesystem. A structural test scans
  the crate's sources for raw mutation primitives outside that funnel — and **asserts its own
  proof of life**, that it located the funnel and saw the expected calls inside it, so it cannot
  pass by failing to look (§16 A4).
- Applied blocks are journaled (path, before, after) to the undo stack **before** the write.
- **Read-before-edit ledger:** a mutation requires its file to have been read this session (via
  `read` or `map_drill`). Violation → auto-inject the file as a read, notify, retry the block
  once. This replaces smallcode's compound `read_and_patch` tool with identical effect and less
  surface — **DECIDED**. The one exception: `write` to an existing unread file is *refused*
  rather than auto-injected, because a full-file overwrite has nothing to anchor on.

### 6.4 State Machine (`rusta-core/state.rs`) — phase gates (R5)

Four states (SmallCTL's six phases simplified: author/execute merged into `Editing`, repair
folded into `Verifying` feedback). **Canonical tool registry — 10 tools, DECIDED:** `read`,
`grep`, `glob`, `map_refresh`, `map_drill`, `dispatch`, `ask`, `edit`, `write`, `shell`.

| State | Available tools | Exit gate |
| --- | --- | --- |
| `Exploring` | read, grep, glob, map_refresh, map_drill, dispatch, ask | plan drafted |
| `Planning` | same as `Exploring` | **plan approval** (user y/n; `/auto` approves) |
| `Editing` | all 10 (shell approval-gated, §6.12) | edit batch applied |
| `Verifying` | read, grep, glob, map_refresh, map_drill, shell, ask | validation green → done; red → back to `Editing` |

- Transitions fire only on scaffold events (`PlanDrafted`, `PlanApproved`, `EditsApplied`,
  `ValidationPassed`, `ValidationFailed`, `UserInterrupt`, `LoopEscalated`) — a closed `enum`;
  invalid transitions are impossible by construction (exhaustive match), never silently ignored.
  All 4 × 7 cells are pinned by a matrix test. `LoopEscalated` is §6.6's escalation edge
  (`Editing → Planning`).

  > **Revision (2026-09-13).** v1.1 had no escalation edge, so the implementation re-seeded the
  > machine out of band and did not journal the regression. The §6.10 replay validator checks
  > every `StateChange` against this table, so the *next* journaled transition was rejected as
  > illegal: the session log became unreplayable and — since sessions auto-resume — `rusta`
  > refused to start in that repo for the rest of the day. Making the regression a first-class
  > event fixes both.

- A tool request unavailable in the current state → a 1–2 line corrective note naming the
  current state and the transition that unlocks the tool. The notes are derived from the table
  itself so they cannot go stale.
- `edit`/`write`/`shell` handlers are simply **not registered** in read-only states — `fs::write`
  is unreachable there (enforced by construction, not by prompt). The matrix test pins both
  directions: allowed cells execute; blocked cells return the note *and* leave file bytes
  untouched with no process spawned.
- **DECIDED:** no 2-stage tool routing in v1 — ten one-line tool descriptions fit the < 500-token
  core prompt. Two-stage selection is §14.
- A transport failure is **not** a user interrupt. An abandoned turn keeps its phase, except in
  `Verifying`, whose only exits are the two validation verdicts — there it fires
  `ValidationFailed`, since an abandoned verification is not a passed one, returning to `Editing`
  where work can continue.

**Tool reference** (JSON input keys → behaviour; results truncated per §6.1):

| Tool | Input keys | Behaviour |
| --- | --- | --- |
| `read` | `path`, `from?`, `to?` | numbered file slice, streamed |
| `grep` | `pattern`, `glob?` | case-sensitive regex matches as `file:line: text` |
| `glob` | `pattern` | matching path list |
| `map_refresh` | — | re-render the repo map (§6.5) |
| `map_drill` | `path` + (`name` \| `from`/`to`) | padded definition span (±8 ctx lines) or exact window; credits the ledger |
| `dispatch` | `task` or `tasks` | read-only sub-coders (§6.8) |
| `ask` | `question` | pauses the turn, surfaces the question; the reply returns as the next observation; at most one pending `ask` per turn |
| `edit` | `path`, `search`, `replace` | tool-call form of a §6.3 block — identical apply chain |
| `write` | `path`, `content` | full-file write; existing file requires a prior ledger read |
| `shell` | `command` | §6.12 safety policy |

- **One edit mechanism, two syntaxes:** text SEARCH/REPLACE blocks (§6.3, canonical) and the
  `edit`/`write` tool calls funnel into the *same* apply chain, undo journal and ledger — never
  two implementations. In read-only states either pathway yields the standard corrective note,
  never a silent drop.
- **Read-only Q&A:** a request answered with prose + read-only tools, without drafting a plan,
  simply ends the turn; the state stays `Exploring`. Plan detection is deliberately conservative
  (`Plan:` + a numbered list) so plain answers never trip the gate.

### 6.5 Repo Map (`rusta-repomap`) — AST context compression (R6)

Pipeline (port of Aider `repomap.py` semantics; **the numbers are normative**):

1. **Files:** git-tracked source files (`git ls-files`), filtered to configured languages;
   `.gitignore`d paths excluded. If not a git repo, a recursive walk with built-in ignores.
2. **Tags:** per file, a tree-sitter query (`.scm`, defs + refs, ported from Aider) yields
   `Tag { rel_fname, name, kind: def|ref, line }`. If a file's query yields defs but no refs,
   backfill refs from an identifier word scan so def-only languages still connect to the graph.
3. **Graph:** one node per file. For each identifier defined in `D` files and referenced by file
   `r` with `n_r` references: edge `r → definer` of weight `mul · √n_r`, where `mul` is
   ×10 for an identifier explicitly mentioned by the user; ×10 for a snake/kebab/camelCase
   identifier of length ≥ 8; ×0.1 for an identifier starting with `_`; ×0.1 for one defined in
   > 5 files; **×50 when the referencing file is in the session chat-set**. Identifiers defined
   but never referenced get a self-edge of weight 0.1 so singletons stay rankable.
   > **Revision (2026-09-13).** v1.1 specified `mul / (|D| · n_r)`, which *inverts* Aider's
   > signal: `repomap.py` multiplies by `sqrt(num_refs)` — damping, not inversion. Dividing made
   > heavy use of an identifier push rank *away* from its definer, and the `/|D|` divisor
   > double-penalised common identifiers already covered by the ×0.1 rule. The ×50 chat-file
   > boost was missing entirely.

4. **Ranking:** personalized PageRank — damping 0.85, power iteration until Δ < 1e-6 or 100
   iterations (~40 lines of Rust; no graph crate). Personalization: `100/N` baseline per file;
   `+100/N` if the file is in the chat-set or mentioned by the user; `+100/N` if any path
   component matches a user-mentioned identifier.
5. **Rendering:** definitions in rank order, grouped by file; chat-set files excluded (their
   content is already in context). Per definition: the def line with a small surrounding window
   (1 above, 2 below), header `path/to/file.rs:`; every rendered line truncated to 100 chars and
   prefixed `│`, and **every elided region marked `⋮`**.

   > **Revision (2026-09-13).** v1.1 specified "up to 8 surrounding context lines". Aider passes
   > `loi_pad=0` and shows the def line plus its *parent scopes* — ~1–3 lines per definition, not
   > 17. The ±8 window made a single mid-sized file exceed the whole §7 default budget. The wider
   > pad remains correct for `map_drill` (step 8), which is a zoom, not an overview. Elision
   > markers were missing altogether, so non-adjacent regions rendered as if contiguous —
   > actively misleading for a model composing a SEARCH block.

6. **Token fitting:** estimate map cost by tokenizing ≤ 100 evenly spaced rendered lines and
   scaling by total/sampled characters (Aider's sampling trick — no full tokenization); binary
   search the largest rank-ordered prefix of the **definition** list that fits, dropping the
   lowest-ranked definitions first.

   > **Revision (2026-09-13).** v1.1 said "drop the middle-ranked **files**". File granularity
   > makes the smallest renderable unit one whole file's definitions, so when even one file
   > exceeds the budget the map renders **empty** — which is what happened at the §7 default of
   > 1024 tokens on every real repository tested, while `map_refresh` reported "no source files
   > matched". Fitting over definitions makes the budget bind smoothly.

7. **Cache:** `(path, mtime, size, query_version) → Vec<Tag>`, in-memory only for v1 (restart
   re-scans; native tree-sitter parses are ms-per-file). `QUERY_VERSION` is hand-bumped and
   documented as such — it is not a guarantee that stale tag shapes can never be served.
8. **`map_drill`:** returns one definition's full span or a line window — and credits the
   read-before-edit ledger (§6.3). **DECIDED (2026-09-11):** definition spans are padded ±8
   lines, clamped at file edges, with the constant shared with the renderer so zoom matches the
   overview; the ledger credit makes the drill the model's possible whole view of a region, and
   the lines just outside a span (doc comments, attributes, `impl` headers) are what a SEARCH
   block anchors on. Explicit `from`/`to` windows are the model's own choice and are never
   padded. A capped window's header states the lines **actually returned**, with the truncation
   marker not counted as content.

**Mentions are wired end to end.** Identifiers extracted from the latest user message (the repo
map's own word scan, kebab-case included per the step-3 ladder) reach ranking through the
registry and `/map`. This is pinned by a test rendering at a budget that *binds* — a map that
fits entirely looks the same however it is ranked, so a non-binding budget proves nothing.

### 6.6 Context Manager (`rusta-core/context.rs`) — purity + JIT + compression (R7)

**Core prompt < 500 tokens** (CI invariant, in all four phases):

- persona (1 line)
- current state + exit gate (2)
- 10 tool one-liners + the ```tool syntax example (~150)
- SEARCH/REPLACE example (~80)
- output rules (~80)
- git footer (~20)

Everything else is injected JIT or not at all.

**JIT skill cards.** The starter deck is compiled into the binary from `skills/*.md`; a project
may extend or override it from `<repo>/.rusta/skills/*.md`. Markdown + YAML front matter
(little-coder's schema): `name`, `type: tool|knowledge|recovery`, `triggers` (tool names, error
kinds, keywords), `priority: 1–9`, `token_cost` (declared; CI verifies ≤ 120),
`user-invocable: bool`; the body is a short imperative card. Injection: on trigger match, at most
**2** cards appended as a trailing system note; evicted at task end. Never resident by default.
Trigger matching is word-boundary anchored.

> **Revision (2026-09-13).** v1.1 left the card directory implicit and the implementation read
> `<repo>/skills/`. The shipped deck therefore never loaded for any repository except Rusta's
> own, and a target repo containing an unrelated `skills/*.md` aborted startup. Error-kind
> trigger cues (`edit_failed`, `not_found`, `duplicate_match`, `validation_failed`,
> `test_failure`) are now produced by one shared function, so a card cannot declare a trigger no
> code path emits.

**History compression:** when the assembled prompt would exceed **60%** of the context window
(integer `3/5`, no float drift), summarize the oldest turns into a single `assistant` summary
message, keeping verbatim: the last 3 turns, every applied edit block, and every error-feedback
exchange. `Compressor::apply_summary` is the single code path shared with `Summary`-event replay,
so `/resume` reproduces live compression byte-for-byte.

> **Revision (2026-09-14).** "Keep the last 3 turns verbatim" and "fit the context window" are
> not jointly satisfiable: three `read` observations at the §6.1 cap are ~65k estimated tokens
> against a 32k window, and with three turns or fewer there is nothing to summarize. The
> implementation returned the history unchanged and the oversized request shipped, because
> nothing downstream measured it — over HTTP that is worse than an error, since llama-server and
> Ollama commonly truncate from the *left*, discarding the system prompt and every phase gate
> with it.
>
> Verbatim is therefore **bounded**: when no turn can be dropped, the largest observations are
> clipped — never below 256 tokens, marked in the text — exactly as a sub-coder clips its own
> (§6.8), so every turn stays present with its evidence bounded. The clip loop must make strict
> progress (the marker's own tokens can otherwise defeat it) and is pinned by a termination test.
> The agent additionally refuses to ship silently: an assembled request still over the window is
> reported **both to the user and to the model**, with a remedy.

**Loop mitigation (FAMA-lite)** — a port of SmallCTL's `fama/` mechanics:

- Detectors: (a) a stagnation counter per `tool|args` fingerprint, tripping at ≥ 3 identical
  calls; (b) ≥ 3 consecutive identical fingerprints in history; (c) ≥ 2 identical validator
  outputs in a row; (d) no-op edit (REPLACE == SEARCH). Fingerprints use canonical JSON, so key
  order never forges a "new" call.
- Response: inject a **mitigation capsule** — one imperative line naming the exact next action:
  - `MUTATION REQUIRED: You have read enough. Emit ONE edit block this turn, then verify.`
  - `Do not repeat the same tool call unchanged; use prior output or switch to a different action.`
  - `Use the evidence already in context before reading or running anything again.`
- Budget: ≤ 180 tokens total, ≤ 5 active, deduplicated, expiring after 3 turns. At 2× threshold:
  automatic state regression (`Editing → Planning`) through the `LoopEscalated` edge — journaled
  like any other transition — plus a user notification.

### 6.7 Validation (`rusta-validate`) — Reflexion gate (R8)

Configured per project (`[validate] commands`). After each applied edit batch the validators run
as `sh -c` subprocesses with a per-command wall-clock timeout and capped output. Their output is
formatted for the model — deduplicated, windowed first error → last error with an elision count,
hard-capped at 30 lines, with `--> path:line:col` rewritten to clickable `path:line:col` — and
fed back as an observation **before the user sees results**. The model gets bounded repair
attempts (3, then surface to the user).

Exit gate: `Verifying` passes only when all validators are green. `ValidationPassed` exits
`Verifying → Exploring`; `ValidationFailed` regresses `→ Editing`. Zero-test detection sums the
libtest tally (passed+failed+ignored+measured — deliberately excluding "filtered out", which
verifies nothing) and attaches a write-real-tests capsule even on green.

A validator failure is **never** an `Err`: unstartable commands, timeouts and garbage become
failing *reports* carrying actionable remedies (§6.11) — including the unstartable-cwd case,
which otherwise burns all three repair rounds on an error the model cannot act on. The only
`Error` is config misuse, caught at load.

### 6.8 Sub-Coder Dispatch (`rusta-dispatch`) — actor isolation (R9)

The `dispatch` tool spawns isolated read-only sub-coders (little-coder semantics): input is a
single `task` string or `tasks` — an array of `{label, task}`, **max 4**, each with a distinct
label. Sub-coder toolset = `{read, grep, glob, map_refresh, map_drill}` — read-only, **no shell
in v1 (DECIDED)**. Each sub-coder is a fresh context (core prompt + task) with its own turn cap
(6, then a forced wrap-up); its final message is the report, **≤ 400 tokens** — enforced by
hard clip, with the clip marker charged against the cap, not appended past it.

Reports return labeled: `SUB-CODER "label" REPORT:` followed by the report, in input order.
Sub-coder reads deliberately do **not** credit the main ledger (isolation); auto-inject still
protects the main agent. A backend failure yields a labeled `RESEARCH FAILED` report; a panicked
actor keeps its label.

**The full sub-transcript goes to the session log only — never main context.** This is carried by
the §6.10 `Dispatch` event's `transcript` field. *Cost recorded deliberately:* a four-task
dispatch can append ~250 KB to the session log.

**Embedded-backend constraint:** one loaded model, one inference thread → sub-coder requests
serialize (correctness preserved; true parallelism exists only on the HTTP backend). Documented
in the README; KV-cache snapshots for cheap sub-contexts are §14.

### 6.9 CLI (`rusta-cli`) — Aider-style REPL (R3)

A `reedline` REPL with 13 slash commands: `/add <glob>` (chat-set), `/drop`, `/undo`, `/diff`,
`/map`, `/state`, `/auto`, `/model`, `/backend`, `/skills <name>`, `/resume <file>`, `/save`,
`/exit`, plus `/help`.

`/add` follows Aider's `glob_filtered_to_repo` semantics — `Path(root).glob(pattern)`, so
`/add *.rs` is root-level, not recursive; a directory argument expands to its files, and a
recursive hint is offered rather than silently changing the match depth.

Git: auto-commit after each applied edit batch with the message `rusta: <one-line summary of the
user request>`; `/undo` restores the file bytes from the journal **and** reverts the commit —
`git reset --mixed HEAD~1` fires only when HEAD *is* the recorded rusta sha, never on an
unrelated commit. Graceful degradation to journal-only undo when there is no git repo.

Config: `rusta.toml` discovery cwd → parents → `~/.rusta/`, with flag-over-file-over-default
precedence and §6.11-style actionable errors.

Non-interactive: `rusta -c "prompt"` runs the agent and exits — plan auto-approved, `ask`
headless, **shell denied by default** (§6.12).

### 6.10 Session Persistence (`rusta-core/session.rs`)

`~/.rusta/sessions/<slug>-<UTC date>.jsonl`, append-only, one JSON object per line. Events:
`SessionStart { config, backend }` · `UserMessage` · `AssistantMessage { content }` ·
`ToolCall { name, input }` · `ToolResult { status, summary, truncated }` ·
`EditApplied { path, before_hash, after_hash }` · `ValidationRun { command, exit, summary }` ·
`StateChange { from, to, reason }` · `Summary { covers_turns, text }` · `Commit { sha, message }` ·
`Dispatch { label, report, transcript }` · `BatchBoundary` · `UndoApplied { entries }` ·
`SessionEnd`.

Full before/after text of edits lives in a FNV-1a-keyed sidecar `<slug>.diffs.jsonl` (two records
per edit) so the chat log stays small; a hash mismatch is reported as corrupt-with-remedy.
`StateChange` events are replay-validated against §6.4's own table. `/resume <file>` replays
events to reconstruct messages, ledger, chat-set, undo stack, batch stack and phase.

**Torn-tail repair.** On open, the log and its sidecar are truncated to their last parseable
line. Only a torn *tail* is repaired — corruption anywhere else still fails loudly, because the
events after it would replay against the wrong state.

> **Revision (2026-09-15).** The fourth audit's torn-tail forgiveness fixed *reading* and never
> repaired the file, so `O_APPEND` fused the next event onto the remnant. One append was
> survivable; the second put the fused line mid-file and auto-resume refused to start for the
> repo-day — the exact failure the §6.4 erratum eliminated, re-created by the fix for it (A2).

**Undo is journaled.** `BatchBoundary` delimits batches (never inferred from `Commit`, which
merges separate requests in no-git mode) and `UndoApplied` records what was actually restored, so
replay cannot resurrect undone edits. `rebuild_batches` applies the same confinement filter
replay does — a single source of truth for what the journal contains (A3).

The log is created `0600` and **re-tightened on open**: `OpenOptions::mode` applies only at
creation, and the logs that predate the guard are exactly the logs still in use.

Session write failures are reported once per session rather than silently discarded; a full disk
must not stop journaling in silence, and the report says undo depth may be short.

### 6.11 Error Taxonomy

Each crate defines a `thiserror` enum (`rusta_llm::Error`, `rusta_edit::Error`, …) with variants
the *model* can act on: `SearchNoMatch { path, hint }`, `FileNotRead { path }`,
`ToolBlocked { tool, state }`, `Malformed`, `Io`, `Backend { .. }`. `anyhow` appears only at the
CLI boundary.

**Rule: every error surfaced to the model MUST contain an actionable remedy.** Underlying errors
are wrapped and truncated to 10 lines, with a §6.6 capsule appended when a detector matches.

### 6.12 Shell & Safety Policy

`shell` runs only in `Editing`/`Verifying`, only after interactive approval (y / n / `a` = always
this session; `/auto` implies `a`; non-interactive `-c` denies by default).

Deny-list (regexes, config-extendable): `rm -rf /` and `rm -rf ~`, `sudo`, `git push --force`,
`dd`, `mkfs`, fork bombs, `curl … | sh`, `shutdown`, `reboot`, `cd` out of the repo, and writes
whose target is outside the repo root — redirects, `tee`, and `cp`/`mv`/`ln`/`install`/`rsync`
destinations — with config extensions surfacing compile errors.

> **What "writes outside the repo root" means here, precisely (revised 2026-09-18).** The table
> tests the three spellings of "outside" — absolute (`/…`), home (`~`, `$HOME`, `${HOME}`), and
> `../` traversal anywhere in the path — rather than enumerating system directories, which had
> missed `/tmp/../etc/x`. For `cp`/`mv` it tests the *final* argument, the destination, so
> reading from outside into the repo stays allowed.
>
> **It is a filter against a confused model, not an adversarial boundary, and cannot be made
> into one.** `sh -c` is Turing-complete, so deciding whether a command writes outside the repo
> is undecidable: `X=~/f; echo hi > $X`, `eval 'rm -rf ~'` and `python -c "open('/etc/x','w')"`
> defeat any pattern list. v1.1–v1.0 stated the rule as though the table enforced it, and round
> 8 (A27) found it implemented for a fraction of the spellings. The rule is now stated as what
> it is: a pre-execution heuristic that catches the realistic failure — an 8B model that lost
> track of its cwd — while approval remains the actual control point. Real confinement for
> *mutations* is structural and lives in §6.3's `guarded` funnel; the shell is the one surface
> that fence does not cover, which is why `/auto` + `shell` carries the §17 warning it does. Execution: cwd = repo root; timeout 60 s
(config); output capped per §6.1 and drained incrementally; no PTY — interactive commands are
detected and denied with the remedy "pass flags for non-interactive mode"; minimal environment
via `env_clear` + `PATH`/`HOME`/`LANG` plus the `[shell] env` allow-list.

> **Revision (2026-09-15).** §6.12 had specified "+ config allow-list" since v1.1 with **no such
> key anywhere** — `[shell] allow` is a list of command prefixes that skip approval, a different
> feature with a confusingly similar name. Added as `[shell] env`: **names only**, values still
> taken from the parent environment. It applies to `shell` alone; validators never `env_clear`,
> so they already inherit everything (A9).

**Read-side confinement.** Lexical path checks reject absolute paths and `..`, but a committed
in-repo symlink pointing outside resolves through `fs::read`. `read`, `grep` and `map_drill`
therefore resolve against the repo root as the mutation paths do.

> **This is a guard rail, not a sandbox.** The TOCTOU window between check and use is real and
> documented. `/auto` combined with `shell` is genuinely dangerous; §17 says so plainly.

## 7. Configuration (`rusta.toml`, discovered cwd → parents → `~/.rusta/`)

Parsed with `deny_unknown_fields`; flag-over-file-over-default precedence.

```toml
[backend]
kind = "http"                           # "http" | "embedded"
base_url = "http://127.0.0.1:8080/v1"   # full API root incl. /v1 (§6.2)
api_key_env = "RUSTA_API_KEY"           # optional; read from env, never stored

[backend.embedded]                      # used when kind = "embedded"
model_path = "~/models/qwen3-coder-30b-a3b-q4_k_m.gguf"
ctx_size = 32768
gpu_layers = 999                        # 0 = CPU

[model]
name = "qwen3-coder-30b-a3b"
context_window = 32768                  # HTTP backend; embedded reads GGUF metadata
max_tokens = 4096
temperature = 0.2

[agent]
max_turns = 16
auto_approve = false                    # plan gate + shell

[repomap]
max_tokens = 1024

[validate]
commands = ["cargo check --workspace", "cargo clippy --workspace -- -D warnings", "cargo test --workspace"]

[shell]
timeout_secs = 60
allow = []                              # extra allow-listed command prefixes (skip approval)
deny = []                               # extra deny regexes (§6.12)
env = []                                # env var NAMES forwarded to `shell` (§6.12)
```

## 8. Milestones — all complete

Built in order, each merging only with all acceptance criteria green (fmt, `clippy -D warnings`,
tests, LoC gate). Condensed; the per-milestone evidence trails live in the git history.

| # | Deliverable | Verified |
| --- | --- | --- |
| **M0** ✅ | Workspace skeleton: 8 crates, CI (fmt + clippy + test + LoC gate), `deny.toml`, MIT licence | 2026-09-10 |
| **M1** ✅ | `HttpBackend`: SSE streaming, 3-attempt retry/backoff, 404 `base_url` hints, native `tool_calls` passthrough, mock-server e2e; JSONL session scaffold | 2026-09-10 |
| **M1.5** ✅ | `EmbeddedBackend` (feature `embedded`): GGUF load, chat template + ChatML fallback, one serialized inference thread → mpsc, exact token counts, cancellation flag; default build compiles without cmake | 2026-09-10, live against a real GGUF |
| **M2** ✅ | `rusta-edit`: parser, apply chain, failure feedback, undo journal, ledger. 15-fixture corpus, insta snapshots, seeded fuzz (5,000 parser / 500 engine iterations, zero panics) | 2026-09-10 |
| **M3** ✅ | `rusta-core`: state machine (all 4 × 7 cells pinned), session persistence with hash-keyed diffs sidecar, `/resume` replay, core prompt ≤ 500 tokens in all phases | 2026-09-11 |
| **M4** ✅ | `rusta-repomap`: 7 languages over pinned grammars, def/ref graph + personalized PageRank, word-scan backfill, budget-fitted rendering with `⋮` elision, mtime-keyed cache, `map_drill`. Golden snapshots; every `.scm` compile-tested against its pinned grammar (the §13 grammar-drift guard) | 2026-09-11 |
| **M5** ✅ | `rusta-core/context.rs`: JIT cards (trigger matrix, ≤ 120-token bodies), 60% compression sharing the replay path, FAMA-lite detectors + capsules + escalation | 2026-09-11 |
| **M6** ✅ | `rusta-validate`: real `sh -c` validators with timeout-kill and capped output, ≤ 30-line deduped windowed feedback, 3-repair bound, zero-test capsule, wired to the phase gate | 2026-09-11 |
| **M7** ✅ | `rusta-dispatch` + `rusta-tools`: the 4-state × 10-tool matrix pinned both directions, §6.1 caps everywhere, §6.12 shell policy, 4 parallel sub-coders with serialization proven on the embedded path | 2026-09-11 |
| **M8** ✅ | `rusta-cli`: the §6.1 turn lifecycle in document order, plan gate, repair rounds, git auto-commit/undo, `rusta.toml` discovery, all 13 slash commands, `-c` mode. Acceptance arc on a scripted mock SSE server: plan → approve → read → edit batch → commit → validator red → repair → green → `/undo` restores file *and* reverts commit | 2026-09-11 |

## 9. Testing Strategy

- **Parser corpus** (`crates/rusta-edit/tests/edit_corpus/`): golden SEARCH/REPLACE fixtures —
  clean, fenced, missing markers, chained DIVIDER, `...` elisions, DeepSeek-style fenced
  filenames, duplicate matches, empty-SEARCH new files — each with an expected parse **and** an
  expected apply result; plus a malformed corpus asserting degradation to prose + corrective note.
- **Tool-call corpus:** fenced single, JSON array form, native `tool_calls` passthrough, unknown
  name, malformed JSON → corrective note.
- **HTTP e2e** uses a hand-rolled mock SSE server (tokio + std `TcpListener` + manual SSE chunks)
  — keeps the dependency tree lean versus wiremock.
- **Property/fuzz:** deterministic seeded generators assert the parser never panics and that
  parsing is deterministic; the §6.12 containment invariant is asserted under generated input.
- **Structural tests:** the write-path scanner (§6.3) with its proof-of-life assertion; the
  4 × 7 transition matrix; the 4 × 10 tool matrix.
- **Embedded tests** are `#[cfg(all(test, feature = "embedded"))]` and run in the `embedded` CI
  job; only the *GGUF* e2e tests are additionally `#[ignore]`d, needing a real model via
  `RUSTA_TEST_GGUF`. The default CI run never needs cmake.

  > **Revision (2026-09-13).** The `embedded` CI job ran `cargo check` only, so those unit tests
  > were compiled but never executed and clippy was never enforced under the feature — despite
  > M1.5's acceptance notes citing both. The job now runs check, feature-forwarding check,
  > `cargo test --workspace --features rusta-cli/embedded`, and clippy `-D warnings`.

- **Invariant tests:** core prompt < 500 tokens; skill cards ≤ 120 tokens; forbidden-unsafe lint;
  the §12 LoC gate.

## 10. Dependencies (every entry must earn its place; minimal feature sets)

- `tokio` (rt-multi-thread, macros, sync, fs, io-util, process, signal — `signal` for §6.1 step
  5's Ctrl-C abort; `io-util` because §6.1's caps must bound *memory*, so subprocess call sites
  drain incrementally through `rusta_core::proc::capped_read` rather than `wait_with_output`) ·
  `reqwest` 0.12 (rustls-tls, json, stream; default features off) · `serde`/`serde_json`/`toml`
  (config and tool-call args only — never the edit format) · `clap` 4 (derive) · `anyhow` +
  `thiserror` · `reedline` (default features off) · `futures` (streams).
- `tree-sitter` + per-language crates (rust, python, javascript/typescript, go, c, cpp), pinned
  exact per the verified compatibility matrix.
- `regex` — the `grep` tool (§6.4) and the shell deny-list (§6.12); both mandate regex semantics,
  so it earns its place twice.
- `llama-cpp-2 =0.1.156` — **optional**, feature `embedded` (+ `sampler`); `embedded-cuda` adds
  CUDA offload. Pinned exact: the crate intentionally does not follow semver. Companion
  `encoding_rs 0.8` (same feature) — llama-cpp-2's detokenization API takes an
  `encoding_rs::Decoder` by type, so incremental UTF-8-safe decoding requires the direct dep.
- Dev-only: `insta`, `tempfile`.
- **Explicitly avoided:** async-trait (native async traits / enum dispatch), graph crates
  (PageRank is ~40 lines), axum/actix, ORM/DAL, anything pulling OpenSSL.

## 11. Risks & Mitigations

| Risk | Mitigation |
| --- | --- |
| `llama-cpp-2` API churn (no semver) | Pin exact; all usage isolated behind `rusta-llm`; upgrade deliberately with contract tests |
| tree-sitter grammar drift | Pin grammar crates; every `.scm` compile-tested against its grammar; golden snapshots |
| GPU build complexity (cmake/CUDA) | Default artifact HTTP-only; `embedded`/`embedded-cuda` opt-in; CI builds both graphs |
| Scope creep breaking R1 | §12 LoC gate + §4 scope fences; new features go to opt-in crates |
| Small-model format variance | Canonical fenced-JSON tool calls + native passthrough + conservative drop-with-corrective-note + recovery parser |
| Apply-chain edge cases | Port Aider's proven semantics *exactly* (§6.3), including the disabled-edit-distance decision; fixtures cover duplicate matches, elisions, cross-file retry |
| Small models derailed by agent loops | FAMA-lite detectors + capsules (§6.6), turn caps, state-regression escalation |
| **Remediation introducing regressions** | §16's three acceptance rules; historical rate ~1 defect per 150–600 changed lines |

## 12. R1 Enforcement — `scripts/loc_budget.sh` (CI gate)

Production ≤ **15,000** lines; total including tests ≤ **25,000**. Canonical counting is the
script (`wc -l` on `*.rs`); `tokei` is an optional cross-check. Data files (`.scm` queries,
`skills/*.md`, prompt fragments) are reported separately, never counted.

**Unit tests count as tests, wherever they live.** Each source file is split at its
`#[cfg(test)]` marker: everything above is production, everything from it down is test.

> **Revision (2026-09-13).** The original gate charged unit tests to the *production* budget. Its
> `-not -name 'tests.rs'` clause showed the intent to exclude them, but this workspace puts unit
> tests in a trailing `#[cfg(test)] mod tests` inside each source file, which that clause does
> not match — so ~4,000 lines of tests were counted as production, inflating the figure by about
> a third. Worse than the wrong number is the incentive: under that count, adding a unit test
> consumed production budget, so the cheapest way to stay green was to delete tests. In a project
> whose defects have all been test-coverage defects, that is precisely the wrong pressure.
>
> **Revision (2026-09-15).** The 20,000 total cap was reached. Production is not the pressure —
> it stands at ~12.3k against its unchanged 15,000 — and all the growth is regression tests from
> six audit rounds. Holding 20,000 would have forced the same trade the revision above exists to
> prevent. The **total** cap is therefore raised to 25,000; the production cap is unchanged and
> remains the number that expresses R1's "lean" intent. Should production approach 15,000, the
> answer is §0 rule 4 — an opt-in crate — not another raise.

## 13. Definition of Done — v1

- All milestones merged; CI green including the LoC gate and `clippy -D warnings`.
- The `rusta` binary builds for Linux and macOS, with and without `embedded`; smoke-tested.
- README quickstart for both backends, including hardware/model guidance and the GGUF manual
  test matrix.
- E2E scenarios pass on the mock server.
- Verified: ≤ 15k production / ≤ 25k total lines of Rust; §15 traceability matrix fully checked.

> **Revision (2026-09-15).** §13 previously named two artifacts, `rusta` (HTTP) and `rusta-full`
> (embedded). **`rusta-full` never existed** — there is one `[[bin]] name = "rusta"` and no
> release workflow distinguishing them; `embedded` is a feature on the same binary. The README
> and `rusta-cli/Cargo.toml` claimed it too. Corrected everywhere to what ships.

## 14. Parking Lot (post-v1, separate opt-in crates — never core bloat)

TUI (ratatui) · GBNF strict grammar as default · KV-cache snapshots for cheap sub-coder contexts ·
out-of-tree Aider-Polyglot-style benchmark harness · MCP client support · Observer-style
multi-agent teams beyond dispatch · per-phase model routing (plan on strong, execute on cheap) ·
2-stage tool routing · per-model tool-call format adapters · udiff edit format as an alternative
model dialect.

## 15. Traceability Matrix

Every row ships and is tested.

| Req | Subsystem | Milestone |
| --- | --- | --- |
| R1 | §4 budget, §12 gate | M0, continuously |
| R2 | §6.2 backends | M1, M1.5 |
| R3 | §6.9 CLI | M8 |
| R4 | §6.3 edit protocol | M2 |
| R5 | §6.4 state machine | M3, M7 |
| R6 | §6.5 repo map | M4 |
| R7 | §6.6 context manager | M3, M5 |
| R8 | §6.7 validation | M6 |
| R9 | §6.8 dispatch | M7 |
| R10 | §2, workspace lints | M0 |
| R11 | §6.2 (offline default), §10 (no telemetry deps) | all |

---

## 16. Audit Record — six review rounds

Six full line-by-line audits were run against this specification and the §3 references. Rounds
1–4 were in-repo; round 5 added two independent external reviews by other LLMs; round 6 was a
self-review of round 5's own remediation diff. This section records what was decided, not
merely what was fixed — the fix list is in the git history.

### 16.1 The recurring defect shape

Every round found the same three shapes:

1. **An unfenced path** — a mutation, read or write that bypassed the guard meant to cover it.
2. **An inert safeguard** — a mechanism specified, implemented, documented and unit-tested that
   **no production caller can reach** (`strict_grammar`, the §6.5 mention boosts, §6.8
   transcripts, the §6.12 env allow-list).
3. **A test green on an input production never supplies** — a hand-fed trigger cue, a 17-line
   fixture, a budget that never binds, an exhaustively pinned table with an out-of-band path
   around it.

Each round fixed its instances; each round's fix introduced new ones. Four of round 5's High and
Medium findings were defects **in round 4's remediation**. The measured rate is roughly one
defect per 150–600 changed lines — unremarkable for changes of this size, and visible only
because this project measures it.

### 16.2 The load-bearing lesson: a guard is not verified by the case that motivated it

Round 4 tried to break the cycle structurally: stop enumerating write paths by hand, make a test
do the counting. That was the right instinct, and its failure is the most useful result these
audits produced.

The scanner was confirmed **red** against the seven unfenced calls that already existed and
**green** after they moved into the funnel. That proves it can see *those* calls and nothing
about the ones it exists to catch. It covered four mutation primitives; five more
(`File::create`, `fs::rename`, `fs::copy`, `fs::create_dir`, `OpenOptions`) were added outside
the funnel and it stayed green — while both it and `apply.rs` asserted in prose that this was
impossible. Testing a detector means **injecting what it should catch**; that took one probe.

### 16.3 Reconciling two independent external reviews

Round 5 commissioned two external audits of the same commit. They reached opposite verdicts,
and the disagreement is more instructive than either report.

| | Review A | Review B | Reconciled |
| --- | --- | --- | --- |
| Verdict | "PASS — ship-quality", 0 Medium-or-above | 10 MAJOR, ~45 MINOR | 3 High, 8 Medium, 14 Low |
| Accuracy of cited facts | High — spot-checks confirmed | High — every MAJOR reproduced but one sub-claim | — |
| Failure mode | **Accepted the guards' own claims about themselves** | Over-graded a few MINORs | — |

**Review A was not hallucinating.** Its citations checked out: the tests it named existed at the
lines it gave; its test counts were exact; its Aider-parity claims held (all seven `.scm` queries
byte-identical to the reference; the edge-weight ladder matching `repomap.py`). Its error was
different and more dangerous — it verified that each guard *exists* and read what each guard
*says about itself*, then reported the guard's self-description as a finding. Its load-bearing
claim, that the write-path scanner made a fifth unfenced path un-addable, was **false**, and was
disproved by adding five.

**Review B was substantially correct.** All ten of its MAJOR findings reproduced. Its method —
naming a file, a line and a reproduction for each — held up under re-verification at a rate no
other source matched, including this project's own earlier passes.

**Decision: independent review earns its place, and "the guard says so" is not evidence.** Five
in-repo passes missed ten MAJOR defects that one outside reader found. That is independent review
working, not a cycle spinning.

### 16.4 Findings and resolutions

All 25 findings were resolved. Most are pinned by a regression test; round 8 found that claim
stated without qualification and false in at least one case — the A8 index bound shipped correct
and untested (round 8's A31, recorded in `AUDIT_REPORT.md`), which is exactly the sentence a later reader would rely on to skip
re-verification. Treat "pinned by a test" as a claim to check, not a conclusion. Condensed:

| # | Severity | Finding | Resolution |
| --- | --- | --- | --- |
| A1 | High | A glued `>>>>>>> REPLACE` was written into user source when a block closed mid-stream; reported as a successful apply, auto-committed, no note | The §6.3 rule-4 strip runs on every commit out of REPLACE |
| A2 | High | Torn-tail forgiveness repaired *reading* but never the file, so the second append bricked auto-resume for the repo-day | §6.10 `repair_torn_tail` truncates log and sidecar on open; mid-file corruption still fails loudly |
| A3 | High | `/undo` rebuilt its batch stack from different sources by different rules, and recorded nothing — so one press could revert an unrelated commit, no-git merged separate requests, and replay resurrected undone edits | `BatchBoundary` + `UndoApplied` events; `rebuild_batches` applies the same confinement filter replay does |
| A4 | Med | The structural write-path guard covered 4 of ≥ 12 mutation primitives, had no proof of life, and did not recurse | 12 primitives, recursive scan, proof-of-life assertion; verified by re-injecting five |
| A5 | Med | §6.5's mention boosts were dead — every production `render_map` caller passed empty slices | User-message identifiers threaded through the registry; pinned at a budget that *binds* |
| A6 | Med | §6.8's sub-transcripts were built, documented as session-log material, and dropped — §6.10 had no event to carry them | `Dispatch` carries `Vec<TranscriptLine>`, `#[serde(default)]` so old logs replay |
| A7 | Med | `/resume` duplicated every dispatch report (once in the observation, once as the event) | Replay uses one source; the events remain the audit record |
| A8 | Med | `tools.resize(index + 1)` on a wire-supplied `u32` requested ~309 GB on `"index": 4294967295` — an abort | Bounded at 64, `Error::Malformed` |
| A9 | Med | §6.12's "config allow-list" for the shell environment did not exist in any form | `[shell] env` added — names only |
| A10 | Med | An in-repo symlink pointing outside resolved through `read`/`grep`/`map_drill`, and `map_drill` credited the ledger for it | Read paths resolve against the repo root as mutations do |
| A11 | Med | The main session log was never re-tightened to `0600`; only the smaller sidecar was | `restrict_existing` called on open |
| A12–A25 | Low | `map_drill` header off by one · `grep` clipped lines with no marker · `/add` chat-set lost across `/resume` · clip marker not charged against the 400-token cap · unstartable validator carried no remedy · transport failures journaled as `UserInterrupt` · a dead `f64` constant beside the live integer math · a vacuous CRLF fuzz claim · `read` loaded whole files before capping · the overflow guard told the user but not the model · the non-existent `rusta-full` artifact · §13's undocumented GGUF matrix · 15 swallowed session-write errors · `capped_read` treating a pipe error as clean EOF | All fixed; the substantive ones are folded into §6.1, §6.5, §6.9, §6.10, §6.11, §9 and §13 above |

**Round 6 (self-review of round 5's diff)** caught four regressions before they shipped, one
worse than the finding it came from: bounding `read` with a fixed 512 KiB prefix silently
clamped any window past it — `read(big.rs, from: 30000)` returned line 19830 with `status: Ok`.
Silently wrong content is far worse than the allocation it replaced, because that output is what
a SEARCH block gets anchored on. `read` now streams, preserving `str::lines` semantics exactly
(a trailing newline does not invent an empty line; a file without one still yields its last
line; CRLF stripped; per-line NUL sniff).

### 16.5 Claims examined and **not** upheld

Recorded so they are not re-litigated.

- **"`read` can exceed the 64 KiB byte cap by one full line."** Not reproduced — the loop
  increments the counter *before* the bound check, so the overshooting line is never pushed.
  Measured 65,313 bytes against the 65,536 cap. (The *other* half of that finding — whole-file
  load before capping — was valid and is A20.)
- **"The write-path scanner makes a fifth unfenced path un-addable."** False; disproved by adding
  five. This was the load-bearing claim of Review A's PASS verdict.
- **"`/add` should match at depth."** Investigated and **reversed**: Aider's
  `glob_filtered_to_repo` uses `Path(root).glob(pattern)`, so `/add *.rs` is root-level. The
  normalization that diverged from it was reverted; directory expansion and a recursive hint were
  added instead.
- **The parser is right about CRLF.** `"x\r"` + CRLF normalizing to `"x\r\n"` is correct, not a
  defect — see §6.3.

### 16.6 Standing acceptance rules

These bind every future change (§0 rule 8):

1. **A detector must assert its own proof of life** — that it found what it expects to find — so
   it cannot pass by not looking.
2. **A change to what a data structure contains must be checked against every consumer of that
   structure.** A3 was a second consumer of the undo journal that nobody looked for when entries
   started being dropped from it.
3. **Reject "the guard says so" as evidence.** Verify by execution, never by reading a guard's
   self-description — including this document's.

## 17. Current Status & Known Limitations

**Status at ADR v1.0 (2026-09-15).**

| Gate | Result |
| --- | --- |
| `cargo fmt` | clean |
| `cargo clippy --all-targets -D warnings` | 0 issues, both feature graphs |
| `cargo doc` | 0 warnings |
| `cargo test --workspace` | **292 passed, 0 failed** (304 with `rusta-cli/embedded`; 3 GGUF tests `#[ignore]`d) |
| `scripts/loc_budget.sh` | production **12,337** / 15,000 · tests 8,603 · total **20,940** / 25,000 |

**Rusta is pre-alpha and is not production-ready.** The gates above are real, but they do not
support a stronger claim, for these reasons:

1. **Zero real-model exposure.** *Every completion this system has ever processed was
   hand-written.* All 292 tests are mock-driven; the GGUF tests are `#[ignore]`d for want of a
   model file. The scaffold has never met a real small model's output distribution — which is the
   one thing it exists to handle. This is the single largest gap, and no amount of further
   self-audit closes it.
2. **`/auto` + `shell` is genuinely dangerous.** §6.12 is accurately self-described as a guard
   rail, not a sandbox: the deny-list is a regex table, and the TOCTOU window between path check
   and use is real.
3. **No external users**, so no exposure to the inputs, repositories and configurations that
   production would supply.

**The recommended path forward**, in order: run against a real `llama-server`; turn the resulting
real completions into corpus fixtures (they are the inputs the mocks are standing in for); keep
commissioning independent review, which §16.3 shows finds what in-repo passes do not.

---

*This ADR supersedes the former `DEVELOPMENT_PLAN.md` v1.2 and `AUDIT_REPORT.md`, both deleted on
2026-09-15. Section numbers §0–§15 are preserved verbatim because ~650 in-code citations depend
on them; see the numbering contract at the top of this document.*
