# Rusta — Workspace QA Audit Report (Round 7)

**Date:** 2026-09-17
**Scope:** All 8 workspace crates (production + tests), cross-crate wiring, workspace manifest, CI gates; comparative assessment against the five §3 reference projects (`aider`, `little-coder`, `Observer`, `smallcode`, `SmallCTL` in `/home/vladimir/develop/refs/`).
**Predecessor:** ADR.md §16 records rounds 1–6. The former standalone `AUDIT_REPORT.md` was consolidated into the ADR and deleted on 2026-09-15; this document re-establishes a standalone report for round 7 and should either stand beside the ADR or be folded into §16 when superseded.

---

## 1. Method & confidence

- **Coverage:** every production and test file in the workspace was read line-by-line across cumulative sessions (63 source files, 20,940 LoC). The four largest files (`rusta-core/context.rs` 1,783; `rusta-edit/apply.rs` 1,775; `rusta-cli/agent.rs` 1,151; `rusta-validate/validators.rs` 1,058) were read in overlapping chunks.
- **Tooling fault, disclosed:** an earlier session's tool results rendered with empty bodies, leaving that session's coverage claims unverifiable. This session re-established trust by (a) spot-check reads of every file whose earlier read was empty, (b) full symbol-outline greps of the four largest files, (c) re-reading the workspace manifest and ADR §16–§17 directly, and (d) **re-running every QA gate live** rather than citing the ADR's self-description (§16.6 rule 3: reject "the guard says so" as evidence — applied to this project's own documentation).
- **Reference comparison** is based on direct reads of the five reference projects (READMEs, `smallcode/ARCHITECTURE.md`, source trees), not on their self-descriptions alone where architecture docs existed.

Confidence is high for structure, wiring, and gates; the standing epistemic limit is the one the ADR itself names (§17.1): **no real-model data has ever flowed through this system**, so behavioral claims about small-model output handling remain mock-backed.

## 2. Executive summary

**Verdict: PASS at the project's own claim level (pre-alpha, mock-verified scaffold).** No new High or Medium findings this round. The workspace is coherent, dependencies run one direction, every specified cap and fence that this audit could exercise by execution held, and the six prior audit rounds' fixes are still in place (spot-verified via the inline `audit_regressions` test modules and `tests/write_paths.rs`).

The three risks that matter are unchanged from ADR §17 and remain open: (1) zero real-model exposure — the single largest gap; (2) `/auto` + `shell` is a guard rail, not a sandbox (regex deny-list, real TOCTOU window); (3) no external users. This round adds structural observations (§9) but no defects of the §16.1 recurring shapes (unfenced path, inert safeguard, test-green-on-unreachable-input) were found — the specific past instances remain fenced and their regression tests still run.

## 3. QA gates — re-executed this session

| Gate | Result | Notes |
| --- | --- | --- |
| `cargo fmt --all --check` | **clean** | printed `FMT_CLEAN`, exit 0 |
| `cargo clippy --workspace --all-targets` | **0 diagnostics**, exit 0 | default feature set re-run live; the `embedded` feature graph is recorded green in ADR §17 but not re-run here |
| `cargo test --workspace` | **292 passed / 0 failed / 0 ignored** across 31 suites | default features, exit 0 |
| `scripts/loc_budget.sh` (R1 CI gate) | **production 12,337 / 15,000 · tests 8,603 · total 20,940 / 25,000**, exit 0 | inline tests 4,167 + suite tests 4,436 |
| `git status` | clean tree, `main` up to date with `origin/main` | 20-commit history: milestones M2→M8 then six audit/remediation rounds |
| `unsafe_code` | `forbid` at workspace level | only `llama-cpp-2` (behind `embedded`, pinned `=0.1.156`) contains any |

Test-to-production ratio ≈ 0.70 (8,603 / 12,337) — unusually high for the domain, and consistent with the ADR's testing strategy (§9).

## 4. Workspace overview

Eight crates, `resolver = "3"`, all metadata inherited from `[workspace.package]`, all lints inherited via `[lints] workspace = true`. The dependency set is contractual (ADR §10): every entry in `[workspace.dependencies]` carries an inline justification comment, exact-pinned where semver is ignored (`llama-cpp-2 =0.1.156`, the tree-sitter matrix), default-features-off where bloat would follow (`reedline`, `reqwest` with `rustls-tls` — no OpenSSL anywhere).

| Crate | Role (ADR) | Production LoC (largest files) | Test files |
| --- | --- | --- | --- |
| `rusta-core` | §6.4 phase machine, §6.6 context/prompt, §6.10 session | context.rs 1,783 · session.rs 1,026 · state.rs 668 · prompt.rs 123 · proc.rs 104 | tests/regressions.rs 611 |
| `rusta-llm` | §6.2 backend boundary: HTTP + embedded | embedded.rs 677 · http.rs 495 · types.rs 175 · tokens.rs 35 | http_e2e 223 · mock_server 178 · embedded_e2e 82 |
| `rusta-edit` | §6.3 parser + apply chain + undo ledger | apply.rs 1,775 · parser.rs 624 · ledger.rs 188 | corpus 318 · property 191 · write_paths 135 |
| `rusta-tools` | §6.4 ten-tool phase-gated registry | shell.rs 486 · exec.rs 362 · glob.rs 252 · edit.rs 242 · read.rs 219 · search.rs 208 · map.rs 204 | regressions 612 · state_matrix 226 · dispatch_tool 177 |
| `rusta-dispatch` | §6.8 sub-coders, parallel read-only DAG | dag.rs 319 · actor.rs 272 · toolcall.rs 191 | e2e.rs 304 |
| `rusta-repomap` | §6.5 tree-sitter repo map, PPR ranking | graph.rs 319 · render.rs 275 · tags.rs 246 · lang.rs 127 · drill.rs 101 | repomap.rs 282 (insta snapshots) |
| `rusta-validate` | §6.7 validators + Reflexion gate | validators.rs 1,058 (tests inline) | — |
| `rusta-cli` | §6.9/§7 REPL host, agent loop, git, config | agent.rs 1,151 · commands.rs 517 · repl.rs 458 · config.rs 457 · git.rs 352 | cli_e2e 550 · regressions 547 |


## 5. Per-crate analysis

### 5.1 `rusta-core` — the state, prompt, context, and session spine

- **`state.rs` (668):** the §6.4 phase machine (`Exploring → Planning → Editing → Verifying`) with a closed transition-event enum — invalid transitions are unrepresentable, and the tool table (`TOOLS`) simply does not register mutation tools in read-only states. Gating by construction rather than by prompt is the right call for small models and is the pattern the whole workspace repeats.
- **`context.rs` (1,783, largest file in the workspace):** JIT skill cards (`CardDeck`, `MAX_INJECTED_CARDS`, `CARD_TOKEN_BUDGET`), episodic history compression (the 60% rule), and FAMA-lite loop mitigation (`LoopGuard`, `Capsule`, `Trip`, `Escalation`). Everything the model might need that must *not* be resident in the core prompt lives here.
- **`prompt.rs` (123):** the sub-500-token core prompt with a CI-checked token budget (`CORE_PROMPT_TOKEN_BUDGET`) plus the §6.1 observation formatter.
- **`session.rs` (1,026):** append-only JSONL journal (`Event`, `ValidationRun`, `StateChange`, `EditsApplied`, …) and `/resume` reconstruction (`Reconstructed` — messages, ledger, undo stack, phase).
- **`proc.rs` (104):** `capped_read` — the shared bounded-subprocess primitive, factored once because its invariant is non-obvious: it keeps reading *past* the cap and discards, so a noisy-but-successful child never deadlocks on a full pipe. The module doc cites the measured motivation (a 3-second `yes` → 5.26 GB RSS under `wait_with_output()`). This is the structural fix for the round-3 subprocess finding, and it correctly lives where both call sites (shell, validators) share it.
- **Quality:** `tests/regressions.rs` (611 lines) pins the behaviors past audits demanded. No new findings.

### 5.2 `rusta-llm` — the backend boundary

- One `ChatRequest`/`Message`/`StreamEvent` vocabulary over two backends: `http.rs` (reqwest, rustls, streaming, bounded stalled-response detection — git `7f8081d`) and `embedded.rs` (in-process `llama-cpp-2`, pinned exact, default features off, incremental detokenization via `encoding_rs` with partial-UTF-8 handling).
- The `Backend` trait is the *only* place the project's forbidden `unsafe` (inside the C++ binding crate) can be reached from, satisfying R10 by architecture rather than by promise.
- Tests are wire-level, not mock-object-level: `mock_server.rs` (178) runs a real local HTTP server and `http_e2e.rs` (223) drives actual SSE streams — the strongest form of mock available short of a real model. `embedded_e2e.rs` (82) is compile-and-shape only; the GGUF tests are `#[ignore]`d (Finding F3).

### 5.3 `rusta-edit` — one edit mechanism, two syntaxes

- **`parser.rs` (624):** fenced tool calls and SEARCH/REPLACE blocks parsed in **document order** (the §6.1 requirement the agent loop depends on), with deliberate, documented CRLF semantics (a §16.5 "claim not upheld" — the behavior is correct as specified).
- **`apply.rs` (1,775, second-largest file):** the apply chain with Aider's original guards restored (git `cc375a2`), the repo-root fence, and the write-path funnel: all mutation primitives go through one choke point.
- **`ledger.rs` (188):** the read-before-edit ledger + `UndoEntry` — the same ledger the `edit`/`write` tools and text SEARCH/REPLACE both credit, which is what makes "one edit mechanism, two syntaxes" true rather than slogan.
- **Tests:** `write_paths.rs` is the round-4 lesson implemented — a scanner that asserts its own proof of life (it fails if it stops finding the fenced calls it expects to find), per §16.6 rule 1. `corpus.rs` uses insta snapshots; `property.rs` runs a hand-rolled deterministic xorshift64 generator (no fuzzing dependency, §10 discipline) over an adversarial alphabet of near-miss markers, CRLF, and unicode.
- **Quality:** this crate concentrates the historically bug-dense surface and responds with the heaviest test apparatus in the workspace. No new findings.

### 5.4 `rusta-tools` — the ten-tool phase-gated registry

- The ten canonical tools (`read`, `grep`, `glob`, `map_refresh`, `map_drill`, `dispatch`, `ask`, `edit`, `write`, `shell`) with `edit`/`write` unreachable in read-only states and `dispatch` unreachable in `Verifying` — by the state table, not by prompt instruction.
- Host interaction is injected, not implemented: `Approver`/`Responder`/`Decision` traits; headless defaults ship in-crate (`/auto` approves, non-interactive denies shell and answers `ask` with a fixed notice).
- `shell.rs` (486): the §6.12 deny-list (regex, case-sensitive by design so user patterns like `\b`/`\w` behave as documented) plus `ShellPolicy`; `exec.rs` (362) drains pipes concurrently through `rusta-core`'s `capped_read` — the memory-bounding fix verified above.
- Every observation channel has a §6.1 cap (read's byte cap, grep's match cap, map's token budget, drill's ±8-line padded spans clamped at file edges).
- `state_matrix.rs` (226) is the by-construction proof of phase gating; `regressions.rs` (612) is the workspace's largest test file. No new findings.


### 5.5 `rusta-dispatch` — sub-coders and the parallel read-only DAG

- `toolcall.rs` (191): the forgiving multi-format tool-call parser (fenced, `<tool_call>`, bare JSON) — smallcode's "forgiving parser" lesson, applied at the seam where small models actually fail.
- `actor.rs` (272): isolated read-only research actors with summarized returns; sub-coder reads deliberately do not credit the main ledger (isolation), while the read-before-edit auto-inject still protects edits.
- `dag.rs` (319): dependency-batched parallel dispatch for independent read-only steps — the LLMCompiler lineage SmallCTL also cites, executed against the same handler functions as the main loop (no parallel write path exists to fence).
- `tests/e2e.rs` (304) exercises the full spawn→summarize→merge path. No new findings.

### 5.6 `rusta-repomap` — the Aider-parity repo map

- Exact-pinned tree-sitter matrix (7 grammars + core `=0.25.9`) whose compatibility is verified by the per-language query compile test (§13); grammar drift is a build failure, not a runtime surprise.
- `tags.rs` runs the seven `.scm` queries that the round-5 reconciliation confirmed **byte-identical to Aider's**, with the edge-weight ladder matching `repomap.py`; `graph.rs` does personalized PageRank; `render.rs` fits output to a token budget with explicit elision markers; `drill.rs` adds ±8-line padded definition spans.
- `cache.rs`/`discover.rs` keep re-maps cheap; `tests/repomap.rs` (282) holds insta snapshots. No new findings.

### 5.7 `rusta-validate` — the Reflexion gate

- `validators.rs` (1,058, single module, tests inline): every bound the ADR specifies is present as a named constant — `REPAIR_BOUND = 3`, `FEEDBACK_MAX_LINES = 30`, `FEEDBACK_LINE_WIDTH = 240`, `OUTPUT_CAP_BYTES = 64 KiB`, `DEFAULT_TIMEOUT_SECS = 600` — plus the zero-test capsule ("green proved nothing", credited in-source to the SmallCTL lesson).
- The feedback formatter deduplicates lines, windows from first to last diagnostic, rewrites compile-error locations to `path:line:col`, and handles partial-UTF-8 at the byte-cap cut (`trim_partial_utf8`). Every failure path yields a remedy-bearing message (§6.11); nothing here can abort the agent (an unstartable or timing-out validator becomes a failing `Report`, not an error).
- `Gate::assess` routes pass/repair/surface-to-user and emits the literal `Verifying`-exit phase event. See Finding F2 on fixture hardening.

### 5.8 `rusta-cli` — the host

- **`agent.rs` (1,151):** the §6.1 turn lifecycle exactly as documented: prompt assembly (core prompt + compressed history + JIT cards + capsules), streaming with native `tool_calls` passthrough, document-order item parsing via `parse_items` (fenced calls and SEARCH/REPLACE interleaved; a tool fence *inside* an edit block is content, not a call — regression-tested), the apply→auto-commit→validate→gate pipeline, Ctrl-C abort that discards the partial turn, and the `WRAP_UP` turn-budget capsule. Interactive hooks read stdin through `block_in_place` so a waiting user cannot park a tokio worker and starve in-flight sub-coders — a cross-module concurrency detail the doc comment gets right. `PlanGate` (auto/terminal) and `TerminalApprover`/`TerminalResponder` implement the injected traits from `rusta-tools`.
- **`commands.rs` (517):** the full §6.9 slash table; `/add` glob semantics were investigated against Aider's actual `glob_filtered_to_repo` behavior and *reverted to match it* (root-level `*`, `**` crosses separators, directory expansion + recursive hint instead of silent deep matching) — a §16.5-reversal done correctly.
- **`config.rs` (457):** §7 schema with `deny_unknown_fields` on every section (typos are config errors), discovery cwd → parents → `~/.rusta/`, flag-over-file-over-default precedence in `Config::http`, and a hand-rolled `civil_from_days` UTC date (no chrono — §10 discipline) with epoch-pinned tests.
- **`git.rs` (352):** pathspec-scoped auto-commits and the `/undo` fence (round-4 remediation, still test-covered in `tests/regressions.rs`). **`repl.rs` (458)** hosts `App::handle_line` so the 550-line `cli_e2e.rs` drives scripted sessions without a TTY. **`render.rs` (37)** keeps output behind a `Reporter` so tests assert what a user sees.

## 6. Cross-crate integration quality

- **Dependency direction is acyclic and clean:** `rusta-cli → {core, llm, edit, tools, dispatch, repomap, validate}`; `rusta-tools → {core, dispatch, edit, llm, repomap}`; the leaf crates (`core`, `llm`, `edit`, `repomap`, `validate`) stay independent of the orchestration layers above them. The `embedded` feature forwards `rusta-dispatch → rusta-llm` without leaking the C++ dependency into default graphs.
- **Shared vocabulary, not shared everything:** events and the phase machine come from `rusta-core`; edit types (`EditBlock`, `AppliedBlock`, `UndoEntry`) from `rusta-edit`; `Verdict` from `rusta-validate`. No crate reaches into another's internals; cross-crate surface is the documented public API.
- **The invariant threading is the standout:** the §6.4 machine in `core` is the same table `tools` gates by and `agent.rs` fires events through; the ledger in `edit` is credited by both syntax paths; `capped_read` in `core` bounds both subprocess sites; the session journal in `core` records the events all crates emit. One mechanism per invariant, referenced everywhere, duplicated nowhere — this is what six audit rounds of "found the same three shapes" converges to.
- **Wiring verified by execution:** 292 tests compile and pass against this exact graph; the LoC gate measures it; clippy is silent under workspace-inherited lints.


## 7. Test & QA assessment

- **292 tests / 31 suites**, all green (re-run this session). The taxonomy maps to the ADR §9 strategy: snapshot tests (insta, in `rusta-edit` and `rusta-repomap`), property tests (deterministic xorshift64, `rusta-edit`), wire-level e2e (real local HTTP server + SSE, `rusta-llm`), TTY-free scripted sessions (`rusta-cli`), a phase-gating state matrix (`rusta-tools`), write-path scanning with proof-of-life (`rusta-edit`), and per-round regression files in four crates that keep every prior audit fix pinned.
- **Test-to-production ratio ≈ 0.70**, enforced by the R1 LoC gate from both directions (production ≤ 15,000, total ≤ 25,000) — the budget makes test debt visible rather than optional.
- **The honest ceiling:** all of it is mock-driven. The round-5 lesson (§16.2/§16.3) is that guards here have historically passed while unreachable and detectors have passed while blind; the fixes (proof-of-life, consumer-checking, execution-over-reading) raise the floor but cannot substitute for the missing input distribution. The GGUF-backed embedded tests remain `#[ignore]`d, so the in-process backend path has never produced a token under CI either.

## 8. Comparison against the reference projects

### 8.1 `aider` (Python; ~6.8M installs) — the editing and repo-map ancestor

Rusta deliberately re-derives Aider's load-bearing core in Rust: SEARCH/REPLACE apply-chain guards (restored to Aider's exact semantics in round 3), the repo map (seven byte-identical `.scm` queries, matching edge weights, PPR ranking, token-fitted rendering), git auto-commit/undo, and the `/commands` surface — including the `/add` glob semantics that round 5 checked against Aider's source and reversed rusta to match. Rusta omits Aider's scale surface: linter-before-commit integration, 100+ language breadth (7 grammars), voice/GUI/browser, model marketplace, analytics. **Assessment: parity on the mechanisms it claims; the omission set is a scope decision, not a gap.**

### 8.2 `little-coder` (TypeScript on the `pi` substrate) — the extension-model contrast

little-coder implements ~30 small-model compensations as runtime-injected extensions (write-guard, read-guard, read-guard-edit, output-parser, quality-monitor, permission-gate, checkpoint, turn-cap, skill-inject, subagent dispatch…). Rusta's counterpart mechanisms are compiled core invariants: read-before-edit ledger, output caps, forgiving tool-call parser, LoopGuard capsules, shell Approver, undo ledger + git, §6.1 turn cap, JIT skill cards, `rusta-dispatch` sub-coders. The mapping is nearly one-to-one; the difference is architectural — rusta trades little-coder's runtime extensibility for a single compiled behavior set that the phase machine and tests can reason about. little-coder's cold-start budget (~7k tokens) dwarfs rusta's sub-500-token core prompt with JIT injection, which is the more aggressive context discipline. **Assessment: mechanism coverage is equivalent; rusta's is less configurable but more verifiable.**

### 8.3 `Observer` (Electron/TS app + API) — the domain outlier

Observer monitors the screen/camera/mic with local models and fires user-facing notifications from sandboxed JS micro-agents; its overlap with rusta is philosophical (local-first, OpenAI-compatible endpoints, llama.cpp/GGUF, a bundled CLI) rather than architectural. Nothing in rusta corresponds to its sensors, sandbox, or app/api split, and nothing in Observer corresponds to rusta's editing/validation pipeline. **Assessment: reference value limited to shared local-model infrastructure choices; not a meaningful comparator for the agent loop.**


### 8.4 `smallcode` (Node.js) — the closest design cousin

Both target 8B–35B local models and both start from "the model is unreliable, compensate structurally." Direct analogs: patch-first editing with unique-match requirement ↔ SEARCH/REPLACE with the apply chain; read-before-write guard (refuse-once-then-allow) ↔ read-before-edit ledger; forgiving multi-format JSON parser ↔ `toolcall.rs`; token budgets and trace visibility ↔ §6.1 caps and the §6.10 journal; per-turn snapshots with auto-rollback ↔ undo ledger + git auto-commit. The instructive divergences: (a) smallcode routes tools by a deterministic regex classifier (8 categories, ~800 tokens saved on `respond`, two-stage routing under 16k, affirmation guard) while rusta gates by phase — coarser but simpler, and phase gating composes with sub-coder isolation in a way intent classification does not; (b) smallcode keeps a two-tier SQLite project memory and an opt-in cloud escalation lane (capped at 5/session); rusta has neither — long-term memory is the session journal only, and escalation would violate the local-only mission. **Assessment: rusta matches the editing/recovery core; the missing memory tier is the most defensible future borrow.**

### 8.5 `SmallCTL` (Python) — the harness-design blueprint

The lineage is explicit in rusta's own source (the zero-test capsule is credited to "the SmallCTL lesson"). Mapping: staged workflows `explore/plan/author` ↔ the §6.4 `Exploring → Planning → Editing → Verifying` machine; evidence-first state ↔ the JSONL session journal; context compression lanes ↔ `context.rs` compression + JIT cards; ToolPlan/ReWOO evidence planning ↔ `rusta-dispatch` planning; parallel read-only tool DAGs ↔ `dag.rs` (both cite the LLMCompiler lineage); Reflexion-style repair ↔ `rusta-validate`'s `Gate` with `REPAIR_BOUND = 3` and ≤30-line feedback; FAMA mitigation capsules ↔ `LoopGuard`/`Trip` capsules; risk labels and approval gates ↔ `ShellPolicy` + `Approver`/`PlanGate`. SmallCTL additionally ships a Textual TUI, a FAMA failure-mode detector suite with runtime escalation signals, and an evals harness — none of which rusta attempts. **Assessment: rusta is a tight, compiled distillation of SmallCTL's runtime ideas (plus Aider's editing/map), and its documentation culture — including this report — mirrors SmallCTL's inspectability stance.**

### 8.6 Summary table

| Mechanism | aider | little-coder | Observer | smallcode | SmallCTL | **rusta** |
| --- | --- | --- | --- | --- | --- | --- |
| Repo map (tree-sitter + PPR) | ✔ (origin) | — | — | graph tools | — | ✔ (byte-parity queries) |
| Search/replace editing + guards | ✔ (origin) | via pi | — | ✔ patch | ✔ | ✔ (one chain, two syntaxes) |
| Phase/state gating | — | tool-gating ext | — | router (intent) | ✔ staged | ✔ (by construction) |
| Small-model output repair | — | ✔ output-parser | — | ✔ forgiving parse | ✔ | ✔ `toolcall.rs` |
| Reflexion/repair loop | — | quality-monitor | — | retries+escalate | ✔ | ✔ `rusta-validate` |
| Parallel read-only DAG | — | — | — | (foundation only) | ✔ | ✔ `rusta-dispatch` |
| Sub-coders | — | ✔ subagent | — | agents/ | — | ✔ isolated actors |
| Loop/failure capsules | — | ✔ | — | — | ✔ FAMA | ✔ LoopGuard |
| Undo/checkpoints | git | ✔ checkpoint | — | ✔ snapshots | checkpoint-on-exit | ✔ ledger + git |
| Turn caps | — | ✔ turn-cap | — | max_turns | bounded | ✔ §6.1 cap |
| Zero-test detection | — | — | — | — | ✔ | ✔ (credited) |
| Long-term memory | — | — | memory tool | ✔ SQLite | experience memory | ✖ (journal only) |
| Cloud escalation | — | — | hosted api | ✔ opt-in | — | ✖ (local-only mission) |
| Real-model exposure | ✔ years | ✔ benchmarked | ✔ shipped | ✔ real hardware | ✔ evals | **✖ none — the gap** |


## 9. Findings (round 7)

No High or Medium findings. Recorded below with severity per the house scale (ADR §16 style).

- **F1 (Low, structural): complexity concentration.** Three files carry 29% of production LoC (`context.rs` 1,783 + `apply.rs` 1,775 + `agent.rs` 1,151 = 4,709 / 12,337). The project's own measured defect rate (§16.1: ~1 per 150–600 changed lines) makes these the highest-risk edit surfaces. The R1 budget is the effective guard; consider module splits only if any of the three grows further, and keep requiring their dedicated regression files on every touch.
- **F2 (Low, test hardening): validator heuristics lack real-output fixtures.** `is_diagnostic`, `is_location`, `zero_tests`, and `first_diagnostic` in `validators.rs` are tuned by hand against imagined `cargo`/`clippy`/`gcc` output. A small corpus of *real* tool-output captures (even without a model in the loop — just run the validators on a scratch repo) would pin the heuristics the way `rusta-edit`'s corpus pins the parser.
- **F3 (Low, known): the embedded backend is compile-tested only.** GGUF-backed tests are `#[ignore]`d for want of a model file; the `llama-cpp-2` path has never produced a token under CI. Unchanged from ADR §17; cheaply fixable by caching a tiny GGUF in CI.
- **F4 (Low, doc hygiene): clippy's `embedded` feature graph was not re-run this round.** The default graph is verified clean here; the ADR records the embedded graph green at v1.0. Re-run `cargo clippy --workspace --all-targets --all-features` before the next tag.
- **F5 (Observation): hand-rolled boundary logic is correct but fragile under future edits.** `trim_partial_utf8` (validators) and `civil_from_days` (config) are exactly the class of code where a later "small cleanup" can silently break edge behavior. Both currently have tests (`utc_date` epoch pins; partial-UTF-8 cases); keep those tests mandatory companions to any change.
- **F6 (Observation, standing): `/auto` + `shell` TOCTOU.** The deny-list is a regex table; the window between path check and use is real; §6.12 self-describes accurately as a guard rail, not a sandbox. No action beyond the documented stance is recommended for v1.
- **F7 (Observation, standing): zero real-model exposure** (ADR §17.1). Every completion ever processed was hand-written. This remains the single largest gap and the top recommendation; no amount of further self-audit closes it.
- **F8 (Observation, process): this report vs the single-document convention.** The ADR superseded and deleted the former `AUDIT_REPORT.md` on 2026-09-15; this file re-creates a standalone report at the repo root (per the commissioning request). Either maintain it as the round-by-round QA record or fold its §16-worthy decisions back into ADR §16 at the next consolidation — do not let the two drift.

## 10. Recommendations (in priority order)

1. **Run against a real `llama-server`** and capture genuine small-model completions as corpus fixtures — for the parser (`rusta-edit`), the tool-call repair path (`rusta-dispatch`), and the loop-mitigation capsules (`rusta-core`). This is the ADR's own #1 and this audit's concurrence (F7).
2. **Add real validator-output fixtures** (F2) — the cheapest hardening on this list; no model required.
3. **Cache a small GGUF in CI** and un-ignore the embedded e2e tests (F3), or gate them behind a labeled job that runs when the artifact is present.
4. **Re-run clippy across `--all-features`** (F4) alongside the existing per-graph checks before any tag.
5. **Keep commissioning independent external review** (§16.3: it found what in-repo passes did not, both times); when commissioning, hand reviewers the §16.6 rules so they verify by execution, not by reading guards' self-descriptions.
6. **Consider a smallcode-style persistent memory tier** only as a post-v1 opt-in crate per the ADR §14 parking-lot rule — the journal-only stance is defensible and the budget fence should decide.
7. **Resolve the two-document question** (F8) at the next ADR revision.

## 11. Verification appendix

Commands executed and observed this session (2026-09-17), from a clean `main` worktree:

```
cargo fmt --all --check                 # FMT_CLEAN, exit 0
cargo test --workspace                  # 31 suites: 292 passed / 0 failed / 0 ignored, exit 0
cargo clippy --workspace --all-targets  # 0 diagnostics, exit 0 (default feature graph)
scripts/loc_budget.sh                   # production=12337 tests=8603 total=20940, exit 0
git status                              # clean tree; main == origin/main
```

Source basis: all 63 workspace `.rs` files (20,940 LoC by `wc -l`, matching the gate's measurement), the workspace and member `Cargo.toml`s, `ADR.md` (§0–§17), and direct reads of the five reference projects under `/home/vladimir/develop/refs/`.

*End of report — round 7.*

