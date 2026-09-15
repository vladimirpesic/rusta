# Rusta — Full Code Review, Quality Analysis & QA Audit (5th pass)

**Date:** 2026-09-15 · **Auditor:** independent full-read audit (plan-approved scope)
**Normative baseline:** `DEVELOPMENT_PLAN.md` v1.2 (incl. all recorded errata 2026-09-13/14)
**References consulted:** Aider, little-coder, smallcode, SmallCTL, Observer (`/home/vladimir/develop/refs/`)
**Commit audited:** `b58eb41` (HEAD → main, clean tree)

---

## 1. Executive summary

**Verdict: PASS — ship-quality.** Every production `.rs` file across all 8 crates was read
line-by-line (≈11.8k production lines, ≈7.7k test lines), all QA gates were re-run locally,
each of the four prior audit rounds' claimed remediations was re-verified against the code,
and the spec's reference-parity claims (Aider apply chain, Aider repomap weights, little-coder
dispatch/card schema, SmallCTL capsule mechanics) were spot-checked against the reference
sources themselves.

Findings this round: **0 Critical, 0 High, 0 Medium, 3 Low, 8 Info/Watch.** No correctness,
security-of-mutation, or spec-compliance defect was found. The three Low findings are
hygiene/resume-fidelity gaps at the edges of the session subsystem; none can corrupt a repo,
escape the workspace, or derail the agent loop. This is the first audit round whose findings
list contains no Medium-or-above entry — consistent with four completed remediation rounds
whose fixes all still hold (§5).

The codebase's defining strength is that its invariants are *enforced by construction and
pinned by tests that assert the invariant, not the happy path*: the single `guarded()` mutation
funnel plus a structural test that scans for raw FS calls (`tests/write_paths.rs`), the
closed 4×7 transition table with an exhaustive pin test, the corpus-agreement test across both
parser entry points, and property tests that assert the §6.12 containment invariant under
arbitrary input. The recurring historical failure mode ("tests feed inputs production never
supplies") is now systematically countered at every seam this audit probed.

---

## 2. Scope & method

| Phase | Activity | Coverage |
| --- | --- | --- |
| 1 | Normative requirements extraction | DEVELOPMENT_PLAN.md read end-to-end (§0–§15, all errata) |
| 2 | Line-by-line source read | all 8 crates, dependency-bottom-up: rusta-edit → llm → repomap → core → validate → dispatch → tools → cli; all 10 test suites; `skills/*.md`; `rusta.toml.example`; CI workflow; `deny.toml`; `scripts/loc_budget.sh` |
| 3 | Reference parity spot-checks | Aider `editblock_coder.py` + `repomap.py`; little-coder `skills/tools/{dispatch,read}.md`; SmallCTL `fama/capsules.py` (read-only refs) |
| 4 | QA gates | fmt, clippy `-D warnings`, `test --workspace`, LoC gate, `cargo doc`, `--features embedded` check **and tests**, release build (cargo-deny not installed locally; CI job exists) |
| 5 | Wiring/integration analysis | ledger/journal/session/state consistency across resume; §6.10 event coverage; agent-loop lifecycle vs §6.1 |

---

## 3. QA gate evidence (re-run 2026-09-15, cargo 1.98.1 / rustc 1.98.1)

| Gate | Result | Detail |
| --- | --- | --- |
| `cargo fmt --all --check` | ✅ | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ | exit 0 |
| `cargo test --workspace` | ✅ | **273 passed, 0 failed, 0 ignored** across all binaries |
| `cargo test -p rusta-llm --features embedded` | ✅ | 23 lib tests (incl. all 12 embedded unit tests) + 3 GGUF e2e correctly `#[ignore]`d; 9 http e2e |
| `cargo check -p rusta-cli --features embedded` | ✅ | feature forwarding compiles (cmake present) |
| `bash scripts/loc_budget.sh` | ✅ | production=11,817 · tests=7,686 (inline 4,177 + suites 3,509) · total=19,503 — vs ≤15,000 / ≤20,000 |
| `cargo doc --workspace --no-deps` | ✅ | 0 warnings |
| `cargo build --release` | ✅ | 17.4 s, thin-LTO profile |
| `cargo deny` | ⚠️ not run locally | cargo-deny not installed on this host; the CI `deny` job (EmbarkStudios action, `all-features = true`) covers it |
| CI workflow audit | ✅ | ubuntu+macos matrix (fmt/clippy/test/LoC), embedded job runs **check, feature-forwarding check, tests, clippy** — the §9 erratum's fix is in place |

**Invariant tests re-confirmed green:** core prompt < 500 estimated tokens in all four states;
shipped skill cards ≤ 120 tokens (declared and estimated); every embedded `.scm` query compiles
against its pinned grammar (§13 drift guard); all 28 `(State × PhaseEvent)` cells pinned.

---

## 4. Findings

Severity: Critical (exploitable/data-loss) · High (wrong behavior, mainline path) ·
Medium (wrong behavior, edge path / spec violation with user impact) · Low (hygiene,
fidelity, minor drift) · Info (observation, no action required) · Watch (trajectory).

### F-1 [Low] — An existing session *log* (not sidecar) is never re-tightened to 0600

`restrict_existing()` exists precisely because "`OpenOptions::mode` applies only when the file
is created" and old logs "are exactly the logs still in use" (its own doc comment,
`rusta-core/src/session.rs:154-176`). It is invoked exactly once — in `record_edit`, for the
**sidecar** (line 244). `Session::open` (line 204) never calls it for the main log, so a log
created before the 0600 guard (or under a looser umask) stays group/world-readable for the
entire session even though it is auto-resumed and appended to. The sidecar gets tightened; the
larger of the two files does not.

*Impact:* same class as the M4 finding the guard was written for (session logs carry whole
file contents, tool observations, validator output), but only for pre-guard files, and only
read-exposure.
*Remediation:* one line — `restrict_existing(&path);` in `Session::open`, after `replay()`.

### F-2 [Low] — `/add` chat-set membership does not survive `/resume`

`/add` credits the live ledger directly (`commands.rs:90-95`) and journals a synthetic
observation named `"session"` (`commands.rs:112`). Replay credits the ledger **only** from
successful `read`/`map_drill` `ToolResult`s and `EditApplied` events
(`session.rs:331-341, 344-351`). After restart, the /add'ed files are gone from the read-set:
repo-map chat-file steering is lost until re-read, and an edit re-triggers the (safe,
self-healing) auto-inject. The user-facing model note ("the user added N file(s)…") *does*
replay, so the model is told about a set the ledger no longer reflects — a fidelity gap, not a
safety gap.

*Remediation options:* journal each `/add` batch as `read` ToolCall/ToolResult pairs (zero
schema change; slightly blurs provenance), or add a dedicated session event, or document the
resume behavior in `/add`'s output. Lowest-risk: the first.

### F-3 [Low] — Read-path tools follow in-repo symlinks outside the workspace

`safe_rel` (`rusta-tools/src/exec.rs:93-115`) is lexical: it rejects absolute paths and `..`,
but a *committed symlink* `notes → /home/user/secrets` resolves through `fs::read` for
`read`/`grep`/`map_drill`, and `map_drill` will even credit the ledger for it. All
**mutation** paths are fenced by `contains_path` (third-audit F13 still holds — the ledger
outside target is refused as a write target, `apply.rs:473-475`, pinned by corpus + property
tests), so this is information-flow only: outside-repo file content can enter the model context
and the (0600) session log. §6.12 governs mutations; the read side is ungoverned by the plan.

### F-4 [Info] — §6.11's letter vs practice: `rusta-edit` has no `thiserror` Error enum

§6.11 says "Each crate defines a thiserror enum (`rusta_llm::Error`, `rusta_edit::Error`, …)".
In practice rusta-edit returns `io::Result` + `FailureReason` inside `ApplyReport` — the right
design, since §6.3/§6.11 make edit failure a *model-facing observation*, not an error. Every
other crate has its enum (`rusta_llm::Error`, `rusta_core::Error`, `rusta_validate::Error`,
`rusta_tools::Error`, `DrillError`, `TaskSetError`). The plan's example list, not the code, is
what is stale. No action beyond a plan footnote.

### F-5 [Info] — Truncation-marker wording varies from §6.1's template

§6.1 specifies `… [truncated N lines/bytes]`. Implementations vary per surface:
`… [truncated]` (llm error clip), `[... N more lines truncated (caps: 2000 lines / 64 KiB)]`
(read), `[... output truncated]` (shell), `[… clipped at 400 tokens — narrow the range…]`
(dispatch/context), `… N lines elided (first→last error)` (validate). Every surface carries an
explicit marker and, where a count is meaningful, the count — the contract's substance holds;
only the letter differs. Optionally unify in a later docs pass; no behavior change warranted.

### F-6 [Info] — `gpu_layers` default contradicts its own remedy text

`[backend.embedded]` defaults to and ships `gpu_layers = 999` ("all"), matching §7's example;
but the `Error::ModelLoad` remedy says "keep gpu_layers = 0 unless llama.cpp was built with GPU
support". Benign (CPU-only llama.cpp clamps/ignores `n_gpu_layers`), but the two texts point
opposite ways for the same failure mode. Align the example comment or the remedy wording.

### F-7 [Info] — `try_dotdotdots` separator nuance vs Aider's regex

Aider splits on `^\s*\.\.\.\n`; because `\s*` spans newlines in MULTILINE mode, blank lines
immediately above a `...` line can be absorbed into the separator piece. Rusta's
`split_on_dot_lines` keeps blank lines in content. The divergence appears only for
blank-line-then-`...` shapes and Rusta's reading is the stricter, more predictable one; the
guards the §6.3 erratum demands (single-piece bail, odd-piece equality, count==1) are present
and match Aider exactly (`apply.rs:813-853` vs `editblock_coder.py:190-241`). No action.

### F-8 [Info] — `...`-piece matching is substring-based (faithful to Aider)

Piece pairs apply via `result.find(piece)`/`replace_range` — substring, not line-anchored —
exactly mirroring Aider's `whole.replace(part, replace, 1)`. A piece without a trailing newline
could in principle match mid-line. This is the documented DECIDED parity position; the
wrong-guess protection is the count==1 ambiguity bail. No action.

### F-9 [Info] — glob-tool vs `/add` dot-file asymmetry is documented on one side only

`/add` filters dot-entries (documented in `commands.rs`); the model-facing `glob`/`grep` walk
skips only `.git/target/node_modules/dist`, so `.github/**` etc. are reachable — desirable for
the documented `.github/workflows` case, but the tool-side behavior is not documented anywhere
the model sees. Optionally add one clause to the `glob` one-liner or a card. Cosmetic.

### F-10 [Info] — Aborted turns consume queued skill-card cues

`assemble()` clears `card_cues` before streaming; a backend failure/Ctrl-C turn discards cues
gathered since the last assembly. Self-healing (cards re-fire on the next matching cue or
error); zero safety impact. No action.

### W-1 [Watch] — Total LoC budget is 97.5% consumed

19,503 / 20,000 total (production 11,817 / 15,000 = 79%). Growth is almost entirely *test*
lines (3,509 suite + 4,177 inline): each remediation round has added tests — the right
pressure under the corrected §12 gate — but at the recent per-round delta (~500–800 test
lines) the total gate trips within one to two more rounds. Per-crate production vs §4 targets
(≈, computed independently of the canonical script): core 2,432/2,000 · cli 2,474/1,800 ·
edit 1,553/1,200 · validate 736/400 are over target (justified respectively by the context
manager's clip/loop-guard hardening, the e2e-testable `App` design, the confinement fence, and
validator hardening); llm 1,511/2,000 · tools 1,532/1,600 · repomap 1,126/1,000 ·
dispatch 572/600 are on target. **Recommendation:** decide *now* whether the §12 total gate

---

## 5. Prior-remediation verification (audits 1–4 re-checked)

Every fix claimed by the four prior rounds was located in the code and its pinning test
identified. **All hold.**

| Prior fix (round) | Re-verified how | Status |
| --- | --- | --- |
| Repo-root fence incl. symlink containment (2nd/3rd, F13) | `confine` + `contains_path` + single `guarded()` funnel; `write_paths.rs` scans production source for raw mutators; corpus test refuses symlink-out writes; property test asserts the invariant under arbitrary input | ✅ |
| Undo path fenced (4th — the "fourth write path") | `UndoStack::undo_last` routes through `guarded`; `write_file_is_confined_too`; resume `skipped_edits` handling + user notification | ✅ |
| LoC gate measurement split (2nd) | script splits files at `#[cfg(test)]`; numbers above reproduce | ✅ |
| Embedded CI job actually tests (erratum, §9) | workflow runs check + feature-forwarding check + `test --features rusta-cli/embedded` + clippy; locally: 23 embedded-inclusive lib tests green | ✅ |
| Parser seam: fence-in-edit-body (3rd, F1) | `BlockScan` shares the parser's marker rules in `parse_items`; regression pins fence-inside-REPLACE as content; **corpus-agreement test runs all 16 fixtures through both entry points** | ✅ |
| Subprocess memory bounding (3rd) | `rusta_core::proc::capped_read` used by both shell and validators; drains-past-cap invariant unit-tested (the property that prevents pipe-deadlock-turned-timeout) | ✅ |
| `map_drill` §6.1 caps + honest header retitle (3rd) | `cap_window`/`retitle`; regression asserts ≤64 KiB + truncation flag | ✅ |
| Few-but-huge turn clipping + loop termination (4th, G2) | `clip_oversized` halving with strict-progress guard; `the_clip_loop_always_terminates` constructs the worst case | ✅ |
| One `ask` per turn (4th, G9) | enforced in `submit`'s loop; parse-level + e2e tests (counting responder) | ✅ |
| Escalation as first-class journaled event (2nd, C3) | `LoopEscalated` edge in the closed table; `fire()` journals; replay validator accepts journaled regressions; both directions pinned | ✅ |
| Skill-card directory + shipped deck (2nd, C4a/c) | `SHIPPED_CARDS` embedded via `include_str!`; `.rusta/skills/` overlay; `error_cues` single vocabulary; foreign-`skills/` startup test | ✅ |
| `strict_grammar` removal (erratum, §6.2) | absent from schema, example, and code; `deny_unknown_fields` would catch reintroduction | ✅ |
| §6.5 errata (×√n_r weights + ×50 chat boost; 1/2-line window + elision; definition-granularity fitting) | `graph.rs`/`render.rs` match the corrected plan **and** the Aider source (§6); golden snapshots + budget tests | ✅ |
| Read-window truncation honesty (1st, M2) | window ≠ cap-truncation pinned both ways | ✅ |
| Glob exponential backtracking (1st/4th, H2/F5) | two-level memoization; pathological-pattern timing tests | ✅ |
| §6.4 plan-gate false positives (4th, F10) | `is_plan` word-start cue matching + numbered list; "footsteps" regression | ✅ |

---

## 6. Reference-parity verification (spot-checks against the sources)

**Aider `editblock_coder.py`** — apply-chain order is exact: perfect → whitespace-flexible →
blank-first-line retry (incl. Aider's `len > 2` guard) → `...` elision → cross-file retry →
structured feedback. Aider's fuzzy-edit-distance tail sits *after a bare `return`* in the
reference (deliberately dead); Rusta simply omits it — the correct reading of "Aider
deliberately disables it". `try_dotdotdots`: single-piece bail, odd-separator equality,
count==1 ambiguity bail, empty-part append, `replace(…, 1)` semantics — all present
(`apply.rs:813-853` vs `editblock_coder.py:190-241`). Failure-feedback format matches §6.3's
verbatim-semantics requirement (insta snapshots pinned). `strip_quoted_wrapping`,
`next_is_editblock`, the 3-line filename scan, and `strip_filename` decorations are all
ported. Documented deviations (aligned-line-equality scoring instead of difflib's ratio;
char-boundary guards; char-count length check) keep the pinned behavior deterministic and
panic-free without changing the accepted language.

**Aider `repomap.py`** — edge weight `use_mul * sqrt(num_refs)` ✓; ×50 chat-referencer boost ✓;
mul ladder (×10 mention / ×10 snake-kebab-camel ≥8 / ×0.1 leading underscore / ×0.1 |D|>5) ✓
(`graph.rs:130-156` vs `repomap.py:485-514`); 0.1 self-edges for def-only identifiers ✓;
personalization `100/N` + chat/mention boosts + path-component match ✓ (equivalent after
normalization); PageRank damping 0.85 / 100 iterations / 1e-6 = networkx defaults ✓; dangling
mass redistributed via personalization ✓ (Aider passes `dangling=personalization`);
rank-distribution across out-edges by weight share = `ranked_definitions` ✓; token fitting =
binary search over the largest rank-ordered *definition* prefix ✓ (Aider fits
`ranked_tags[:middle]` the same way). Aider's global `if not references: references = defines`
fallback is replaced by the per-file word-scan backfill — same intent, finer granularity,
documented in §6.5 step 2.

**little-coder** — `skills/tools/dispatch.md` confirms the input shapes (`task` |
`tasks[{label,task}]`, max 4, distinct labels), read-only sub-coders, reports-not-transcripts,
labeled return: all implemented (`dag.rs`, `actor.rs`). Card schema: little-coder's flat
front-matter (`name/type/priority/token_cost/user-invocable`) is carried over with
`target_tool` generalized to `triggers: [...]` — the plan-documented adaptation; strict
parse-then-validate with loud load-time failure matches the "never silently at injection"
requirement.

**SmallCTL `fama/capsules.py`** — `mutation_required` ≈ `mutation_loop_breaker` ("MUTATION
REQUIRED … Emit ONE … then run the verifier"), `repeat_breaker` ≈ `tool_exposure_narrowing`,
`evidence_reuse` ≈ `evidence_reuse_capsule`; the 180-token default budget matches; one-line
imperatives naming the exact next action match §3's "port the mechanics verbatim in spirit".
Rusta's 4-detector set is the documented SmallCTL-derived subset with budget/TTL/dedup caps.

**smallcode** — conservative drop-on-malformed (parser notes, never panics) and the
`read_and_patch` → ledger auto-inject equivalence (DECIDED) both verified in code.

---

## 7. Traceability re-audit (§15 matrix, R1–R11)

| Req | Claim | Audit result |
| --- | --- | --- |
| R1 | ≤15k production / ≤20k total, CI-gated | ✅ gate runs in CI and locally; 11,817/19,503 — see W-1 on trajectory |
| R2 | Dual runtime-selectable backend | ✅ enum dispatch; `--backend`/config precedence (flag>file>default) tested; embedded compiles + unit tests run; the choice is invisible above `Backend` |
| R3 | Aider-style REPL CLI | ✅ reedline, 13 §6.9 commands (+`help`/`quit`), `-c` oneshot with shell denied, double-Ctrl-C convention |
| R4 | Plain-text edit protocol, forgiving parser, proven apply chain | ✅ §6.3 rules 1–6 implemented incl. all errata; Aider parity verified against source |
| R5 | Phase-gated machine + read-before-edit | ✅ closed 4×7 table; mutation tools unregistered in read-only states; corrective notes BFS-derived from the table; auto-inject ledger |
| R6 | Tree-sitter map, budget-fitted, cached | ✅ 7-language pinned grammar matrix + query-compile drift guard; definition-granularity binary-search fitting; (path,mtime,size,version) cache |
| R7 | <500-token prompt, JIT cards, compression, capsules | ✅ CI invariant tests; 4-card shipped deck ≤120 tokens; 60% trigger + keepers + clip fallback; 4 detectors + escalation |
| R8 | Auto-validation Reflexion gate | ✅ §6.7 verbatim: dedup, first→last window, 30 lines, clickable locations, repair bound 3, zero-test capsule, event wiring |
| R9 | Sub-coder dispatch over Tokio | ✅ isolated actors, ≤4 tasks, 6-turn cap, wrap-up, 400-token reports, transcripts to session log only, embedded serialization |
| R10 | `forbid(unsafe_code)` everywhere | ✅ enforced via `[workspace.lints.rust]` inherited by all 8 crates (equivalent compile-time guarantee to the crate-level attribute); the only unsafe is inside llama-cpp-2 behind `embedded` |
| R11 | Offline, no telemetry, no accounts | ✅ only network path is the user-configured backend; no telemetry deps (deny.toml sources block crates.io-fork style surprises); API key from env, never persisted |

§6.10 event-schema coverage: all 12 event types implemented, serialized snake_case-tagged
(round-trip test), replay-validated (`StateChange` against the machine's own table;
`Summary` through the live compression path; sidecar in lockstep with hash verification).

---

## 8. Per-crate chapters

### rusta-edit (1,553 prod / 1,051 test lines)

Faithful Aider port with every §6.3 forgiveness rule and erratum; the standout is the
containment design — `confine` (lexical) + `contains_path` (filesystem/symlink, TOCTOU
documented as hardening-not-sandbox) + the single `guarded()` mutation funnel + the structural
`write_paths.rs` guard that makes a fifth unfenced write path un-addable. Journal-before-write
with rollback-on-failed-write; cross-file retry refuses unconfined/linked-out targets.
Deviation quality is high (char-boundary guards, deterministic similarity). No defects found.

### rusta-llm (1,511 / 594)

Clean enum-dispatch backend; SSE parsing is byte-level and survives chunk splits, CRLF, pings,
UTF-8 boundaries (pinned by e2e). Retry scope is deliberately narrow (connect-errors + 5xx
only) with the reasoning documented — the right call against replaying possibly-in-flight
requests. `read_timeout` as a response-*start* cap (not whole-request) is a considered design.
Embedded: dedicated OS thread, exact tokens, cancellation flag wired through `Backend::stop()`
(which the agent loop calls — the second-audit "flag had no caller" class of bug has its
guard), stop-sequence `DeltaEmitter` with holdback + char-safe slicing, sanitized samplers.
No defects found.

### rusta-repomap (1,126 / 484)

Aider semantics verified point-by-point against `repomap.py` (§6). Rendering hygiene is
excellent: `⋮` elision markers on every non-adjacent boundary including file edges, stale-line
clamp, deterministic output pinned across instances. `split_on_dot_lines` mirrors
`re.split` parity including the trailing-separator empty piece. Watch (not a defect): tag
`name.len() > 128` filter and the merged keyword list are coarse by design and documented.

### rusta-core (2,432 / 1,210)

The strongest crate. `state.rs` — closed transition table, illegal-rejected-loudly, tools as
static registry slices, corrective notes derived from the table itself. `session.rs` —
FNV-1a-keyed sidecar (stable hashing, reference vectors), torn-tail forgiveness on both files,
0600 creation + sidecar re-tighten (see F-1 for the main-log gap), replay reproducing live
context byte-for-byte through shared code paths. `context.rs` — strict card schema, canonical
JSON fingerprints (immune to feature-dependent map ordering), the compressor's
clip-fallback with strict-progress termination, LoopGuard with per-task reset. `proc.rs` —
the drain-past-cap invariant, documented once, used twice. No defects found beyond F-1.

### rusta-validate (736 / 346)

Every §6.7 clause implemented and tested: window budgeting that reserves header lines so
later failures can't starve earlier ones, capsule applied after truncation so it can't be
truncated away, zero-test detection with a matrix covering libtest tallies, filtered-out
counts, "10 tests" boundary, and non-libtest runners; `trim_partial_utf8`'s history (an inert
guard whose test supplied inputs production never gives — caught and fixed by a prior round)
is itself documented as the anti-pattern. No defects found.

### rusta-dispatch (572 / 527)

Compact and correct: validated task sets (≤4, distinct labels, the empty-array error message
fixed to not say "too many"), positional label recovery for panicked actors, input-order
joins, the embedded-serialization gate proven by max-in-flight assertions in both modes.

### rusta-tools (1,532 / 1,130)

The gate ordering (unknown → state → handler) is normative-correct and the state matrix test
asserts the *physical* no-op of blocked cells (bytes unchanged, no process spawned). Caps are
§6.1-exact with honest truncation semantics (window-vs-cap distinction pinned). The shell
policy implements the full §6.12 table plus interactive detection, prefix word-boundary
approval bypass, `env_clear`, and bounded-memory execution. The two-level-memoized glob
matcher is linear-time on adversarial patterns with semantics pinned. `map_drill`'s
header-retitle honesty fix holds. No defects found beyond F-3 (read-side symlink).

### rusta-cli (2,474 / 1,382)

The agent loop implements §6.1 faithfully: document-order interleaved execution via the
`BlockScan`-aware splitter, native `tool_calls` after text items, wrap-up at the cap whose
completion is recorded but not executed, Ctrl-C on both stream and validation rounds firing
`UserInterrupt`, escalation journaling. `finish_batch` ordering (journal → `EditsApplied` →
commit → validate) matches §6.4/§6.9; `/undo` never resets unrelated commits (pinned both
ways); `git commit --only --` protects pre-staged user work. Config schema is strict
(`deny_unknown_fields`) with the shipped example parse-pinned so it can't drift. The
library/`main.rs` split makes every path TTY-free testable. Findings here: F-2, F-10; the
`block_in_place` stdin guards are correctly runtime-flavor-checked.

---

## 9. Wiring & integration analysis

Cross-cutting consistency checks performed (all pass unless a finding is cited):

- **Ledger/journal/session triangle:** every mutation path (`apply_parsed`, `write_file`,
  cross-file retry, `/undo` restore) journals before writing; every ledger credit funnels
  through `record_read` with `confine`; replay rebuilds ledger (read/map_drill results +
  EditApplied), journal (sidecar lockstep), phase (table-validated), and the `/undo` batch
  stack (EditApplied/Commit grouping). Exception: F-2 (`/add`).
- **Turn lifecycle vs §6.1:** actionable item ⇒ execute in document order ⇒ observations ⇒
  next turn; no-items prose ⇒ answer (or plan gate); hard cap ⇒ wrap-up capsule ⇒ return;
  Ctrl-C ⇒ discard partial + `UserInterrupt`; suggested shell commands surfaced-not-run, with
  the model told only when the turn continues.
- **Context assembly:** core prompt + compressed history + JIT cards + capsules + wrap-up,
  with reserved-token accounting for notes and a final overflow report with remedy (the
  left-truncation hazard guard). `Summary` events are journaled exactly when compression ran,
  keeping replay byte-identical.
- **State machine reachability:** `LoopEscalated` fires only from Editing (illegal elsewhere —
  pinned); `ValidationFailed` returns Verifying→Editing so repairs happen where `edit` is
  registered; `dispatch` is unreachable in Verifying; `ask`/`shell` availability matches the
  §6.4 table cells.

## 10. Test-quality analysis (the recurring failure mode)

The known historical failure mode — tests exercising shapes production never produces — was
probed at every seam:

- **Production-path parity:** the corpus-agreement test drives all 16 fixtures through
  `parse_items` (the agent's seam), not just `parse_response`; e2e sessions run the real
  streaming client, real assembly, real git; the embedded-serialization proof uses a delayed
  mock with an in-flight gauge rather than timing luck.
- **Invariant over happy-path:** containment asserted under arbitrary generated input; the
  transition table asserted exhaustive (a 4×7 completeness check inside the test itself); caps
  asserted by content inspection (marker strings, remaining counts), not just booleans.
- **Adversarial fixtures:** pathological glob patterns with timing bounds; 1000-error
  validator output; three asks in one turn; symlink escape; torn log tails; the
  `10 tests` zero-test boundary.
- **Remaining exposure (minor):** GGUF e2e remains env-gated (documented, `#[ignore]`d —
  acceptable); no fuzzing of the tool-call JSON parser beyond unit fixtures (low risk: serde
  never panics and malformed input degrades to notes); the scripted mock servers cover SSE
  edge cases but not, e.g., `[DONE]` without a finish_reason (behavior is defined and safe —
  `Finish(Stop)` default — but untested).

## 11. Prioritized remediation backlog

| # | Item | Effort | Risk |
| --- | --- | --- | --- |
| 1 | F-1: `restrict_existing(&path)` in `Session::open` + test | 1 line + test | trivial |
| 2 | W-1: decide the §12 total-budget policy before the gate trips | decision | none |
| 3 | F-2: journal `/add` credits replayably (or document) | small | low |
| 4 | F-3: fence or document read-side symlink following | small | low |
| 5 | F-6: align the two `gpu_layers` guidance texts | docs | none |
| 6 | F-4/F-5/F-9: plan footnote / marker-wording unification pass | docs | none |
| 7 | F-7/F-8/F-10: no action (recorded as known properties) | — | — |

## 12. Appendix — audit coverage

- **Read line-by-line (production):** all 49 `src/**.rs` files across the 8 crates —
  edit: lib/parser/apply/ledger · llm: lib/types/error/tokens/http/embedded · repomap:
  lib/lang/discover/tags/graph/render/cache/drill · core: lib/error/proc/prompt/state/
  session/context · validate: lib/validators · dispatch: lib/toolcall/actor/dag · tools:
  lib/ask/dispatch/edit/exec/glob/map/read/search/shell · cli: lib/main/render/config/git/
  repl/commands/agent.
- **Read (tests & data):** all 10 `tests/*.rs` suites, `skills/*.md` (4 cards),
  `rusta.toml.example`, `.github/workflows/ci.yml`, `deny.toml`, `scripts/loc_budget.sh`,
  workspace + all crate `Cargo.toml`s.
- **Reference files consulted:** `aider/coders/editblock_coder.py`, `aider/repomap.py`,
  `little-coder/skills/tools/{dispatch,read}.md`, `SmallCTL/src/smallctl/fama/capsules.py`.
- **QA artifacts:** gate outputs from the 2026-09-15 run summarized in §3.

*End of report.*
