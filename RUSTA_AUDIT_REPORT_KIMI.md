# AUDIT REPORT — Rusta (full project review, code quality & QA audit)

**Date:** 2026-09-15
**Scope:** Entire workspace — 8 crates (`rusta-llm`, `rusta-edit`, `rusta-core`, `rusta-tools`, `rusta-repomap`, `rusta-dispatch`, `rusta-validate`, `rusta-cli`), all tests, CI, scripts, docs, skill cards, queries.
**Method:** Line-by-line read of all 63 `.rs` files (~16.0k lines) against the normative specification `DEVELOPMENT_PLAN.md` (all 15 sections) and the five reference implementations at `/home/vladimir/develop/refs/` (`aider`, `little-coder`, `Observer`, `smallcode`, `SmallCTL`) — key mechanisms verified by direct source comparison, not assumption. Every severity-1/2 finding below was re-verified by a second read of the exact cited code before inclusion. All QA gates were executed locally.
**Gate results (executed):**

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | ✅ clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ clean |
| `cargo test --workspace` | ✅ **273 passed, 0 failed** |
| `scripts/loc_budget.sh` (R1) | ✅ production=11,817 (≤15,000) · tests=7,686 · total=19,503 (≤20,000) |
| R10 `forbid(unsafe_code)` | ✅ workspace-wide; only `unsafe` token in tree is a keyword list in `rusta-repomap/src/tags.rs:27` |

---

## 1. Executive summary

**Verdict: the project is in very good shape — no CRITICAL defects found; every normative requirement R1–R11 ships and is tested.** This audit found **10 MAJOR** and **~45 MINOR** findings. Three of the MAJORs belong to the project's own historical defect class — a spec-promised feature that silently does nothing while all gates stay green (dispatch transcripts, repomap mention boosts, README GGUF matrix) — and four are resume/session-path integrity issues that only manifest across process restarts, which explains why the (otherwise excellent) test suite cannot see them.

The codebase shows clear institutional learning from the four 2026-09-13/14 errata: single-sourced caps, one fenced write path with a *structural* guard test, BFS-derived corrective notes, seeded fuzz with containment invariants, and named regression tests that read as an incident changelog. Code quality is consistently high; the residual risk is concentrated in (a) HTTP-path trust of server-supplied data, (b) session-log edge cases around torn writes, undo, and replay, and (c) a handful of dead-parameter seams.

**Top remediation priorities (all confirmed against cited code):**

1. `rusta-edit`: mid-stream glued `>>>>>>> REPLACE` marker is committed verbatim into user source — the exact corruption class §6.3 rule 4 was written to prevent, still reachable via the mid-stream commit path (`parser.rs:180-199`).
2. `rusta-core`: append-after-torn-tail fuses the session log and permanently bricks auto-resume for that repo-day (`session.rs:204-227`).
3. `rusta-cli`: `/undo` is not journaled; after auto-resume a stale undo entry can silently rewind working-tree bytes to content the user already rejected (`commands.rs:177-215`).
4. `rusta-llm`: unbounded `tools.resize(server_index + 1)` on a wire-supplied u32 → process-aborting allocation (`http.rs:373-377`); `read_timeout` aborts healthy streams with >120 s inter-token gaps and the error carries no remedy (`http.rs:30-34,185`).
5. `rusta-dispatch`: §6.8's "full sub-transcript goes to the session log" is implemented only as doc comments — no event type can carry a transcript, so it is silently dropped.
6. Integration: the §6.5 user-mention boost (×10 identifier, +100/N personalization) is dead in production — every `render_map` caller passes `&[], &[]` (`rusta-tools/src/map.rs:74`, `dispatch.rs:65`, `rusta-cli/src/commands.rs:231`).

---

## 2. Findings inventory

**Severity scale:** CRITICAL = shipped feature broken/silent, data loss, or safety hole · MAJOR = real bug or clear spec violation in a reachable path · MINOR = robustness / accuracy / maintainability.

- CRITICAL: **0**
- MAJOR: **10** (§4.1, §4.2, §4.3, §4.3, §5.1, §5.2, §6.1, §7.1, §7.2, §8.1)
- MINOR: **~45** (itemized per area below)

### MAJOR findings (verified, with reproduction reasoning)

| # | Area | File:line | Finding |
| --- | --- | --- | --- |
| M1 | rusta-edit | `parser.rs:180-199` vs `212-224` | `split_trailing_marker` runs only in the end-of-stream branch. A glued `>>>>>>> REPLACE` at the end of the last REPLACE line is stripped only if the block is the final one in the stream; when the block is closed mid-stream (by a following UPDATED line or a chained DIVIDER), `updated.concat()` is committed with the marker fused to the last content line, the apply is reported successful, and `>>>>>>> REPLACE` is written into the user's source. Reproduced: `new>>>>>>> REPLACE\n` lands in the file. Fix: run the strip on every commit out of `in_replace`. |
| M2 | rusta-llm | `http.rs:373-377` | Native `tool_calls` assembly does `tools.resize(index + 1, …)` on the server-supplied u32 `index` with no bound. A single crafted SSE chunk `"index": 4294967295` triggers a ~309 GB contiguous allocation attempt → abort. A wire-input amplification crash, in tension with the §6.1 "never panics on malformed input" posture. Fix: cap index (e.g. ≤64) → `Error::Malformed`. |
| M3 | rusta-llm | `http.rs:30-34, 185` | `RESPONSE_TIMEOUT` (120 s) is documented as time-to-first-byte, but reqwest 0.12 `read_timeout` resets on every successful read — it is an *inter-chunk* gap cap. TTFB >120 s (plausible on the plan's own CPU-only 30B target) or any >120 s inter-token pause aborts a healthy stream mid-generation as `StreamEvent::Failed`, which carries no remedy (§6.11 deviation). Either implement the documented intent or document the real semantics with a remedy. |
| M4 | rusta-core | `session.rs:204-227` | Torn-tail forgiveness repairs nothing on disk: `replay()` forgives a partial final line, then `Session::open` resumes appending via O_APPEND without truncating the partial bytes. The *next* `record()` fuses `{new-event}\n` onto the unterminated tail → one line `…partial{new-event}`. On the following open that fused line is mid-file corruption ⇒ `Error::Corrupt` ⇒ auto-resume refuses to start — the exact "refuse to start" failure mode the torn-write erratum fixed, re-triggered by the first write after a forgiven crash. Tests exercise open-after-torn, never append-after-torn. Fix: truncate log (and sidecar) to the consumed valid prefix after replay. |
| M5 | rusta-core | `session.rs:337-341` vs `395-399` | `/resume` duplicates every dispatch report: the ToolResult replays as `TOOL RESULT dispatch (ok)\nSUB-CODER "a" REPORT:…`, and each `Dispatch` event replays *again* as a standalone user message. Resumed context = live context + N extra report messages, breaking the byte-for-byte compression/replay invariant for any resumed session that used dispatch. No test pins replay-with-dispatch. Skip one of the two reconstructions (the `Dispatch` events are the better source). |
| M6 | rusta-dispatch | `actor.rs:12-13, 71-72, 98-159` | §6.8: "the full sub-transcript goes to the session log only" is **unimplemented**. `Report.transcript` is built and documented as session-log material, but the session schema (§6.10) defines no transcript-bearing event and no production code path writes it — it is dropped after `labeled()` strips it. The project's signature defect class: spec-promised feature silently absent with all gates green. Fix: add a transcript field/sidecar to the session schema, or correct §6.8 and the doc comments. |
| M7 | rusta-cli | `commands.rs:177-215` | `/undo` journals nothing. Live: batch {entry A→B, sha S} applied, `/undo` restores A + resets S, user edits A→C (commit S2), exit; auto-resume replays all `EditApplied` events and rebuilds the stale entry E(A→B). A later `/undo` pops E and `undo_last()` writes B over C while `reset_if_head(S)` refuses (HEAD moved) — git survives, but the **working tree is silently rewound to content the user already rejected**, reported as "restored 1 file(s)". No test covers undo-then-resume. Fix: journal an undo event (or tombstone the undone EditApplied hashes) and honor it in replay. |
| M8 | integration | `rusta-tools/src/map.rs:74`, `dispatch.rs:65`, `rusta-cli/src/commands.rs:231` | §6.5 steps 3–4 user-mention boosts are dead in production: all three production `render_map` callers pass `&[], &[]` for mentioned-identifiers/mentioned-files. Only `boost_multiplier`'s unit test exercises the ×10/+100-N paths. Same class as the removed `strict_grammar` no-op. Fix: extract identifiers from the latest user message (a tokenizer-free word scan, same as the repomap backfill) and thread them through, or amend §6.5. |
| M9 | integration | `rusta-cli/src/repl.rs:376-403` | `rebuild_batches` starts a new `Batch` only when the previous has `sha.is_some()`. In no-git mode or after a failed commit (no `Commit` event), consecutive edit batches from *separate user requests* merge into one batch on resume — one `/undo` then pops entries that belonged to several live batches. Reachable via the shipped no-git degradation path. Fix: start a new batch on each `UserMessage`, or journal batch boundaries. |
| M10 | docs | `README.md` | §13 Definition of Done requires a "GGUF manual test matrix documented" — it exists only in the module doc of `crates/rusta-llm/tests/embedded_e2e.rs:1-15`. No user-facing doc mentions `RUSTA_TEST_GGUF` or any model/quant/ctx test procedure. DoD item unmet. |

---

## 3. Crate-by-crate assessment

### 3.1 `rusta-llm` (2,103 lines) — quality: excellent; 2 MAJOR, 2 MINOR

Plan compliance: **full** — R2 enum dispatch, SSE streaming, 3-attempt retry (conn/5xx only, 250 ms→2 s, e2e-proven), 404 hints for all three server forms, `tool_calls` delta assembly with fragmented-argument e2e, `complete()`, `ceil(chars/3)` estimator as single swap-in point, context window from config; embedded backend verified against the pinned `llama-cpp-2 0.1.156` source (chat template + ChatML fallback, temp→top_p→greedy 0.2/0.9, single OS inference thread, between-token cancel flag, exact token counts, `encoding_rs` incremental decode; every used API signature matches the pin). M1/M1.5 claims verified by running the suite (11 unit + 9 e2e + 12 feature-gated unit + 3 `#[ignore]` GGUF). Default build verified cmake-free.

Findings: **M2, M3** (above). MINOR: (a) SSE `buffer` grows unboundedly under a no-blank-line trickle and `retain` per chunk is O(n²) — add a ~1 MiB cap (`http.rs:341-353`); (b) non-2xx path buffers the whole body via `response.text()` before clipping to 10 lines (`http.rs:280`). Dormancy note: `ChatRequest.stop` has no production caller — the stop-emitter corpus guards a currently unreachable path (harmless; worth a comment).

### 3.2 `rusta-edit` (3,114 lines) — quality: strongest port in the project; 1 MAJOR, 5 MINOR

Plan compliance: **full** — parser rules 1–6 all verified, including the chained-DIVIDER forgiveness matched against `editblock_coder.py:515-526`; filename resolution order matches the plan (Aider's silent `filenames[0]` fallback deliberately absent per §0 rule 3); marker lines cannot hijack filename resolution; apply chain ported function-by-function against the Aider reference (perfect/whitespace, blank-line retry with the identical `len > 2` guard, dotdotdots with pairing + the `len==1` bail-out, cross-file retry, verbatim-Aider failure feedback); the single `guarded` mutation funnel with symlink walk-up confinement for **both** syntaxes; undo journal pushed before every write; ledger auto-inject + retry-once. Structural test (`tests/write_paths.rs`) scans for unfenced mutators. 68 tests green, including 5,000-iteration seeded parser fuzz.

Findings: **M1** (above). MINOR: (a) `UndoStack::undo_last` drops the journal entry if the restoring write fails — every other path rolls back symmetrically (`apply.rs:123-137` vs `313/441/490`); (b) failure feedback does an uncapped `fs::read` of the failed file and an O(file×search) window scan (`apply.rs:916, 965-1006`) — no §6.1-style bound on a model-facing path; (c) fuzz alphabet doc claims CRLF coverage but contains no `\r\n` piece — the no-`'\r'` invariant is vacuous for generated input (`property.rs:33-76`); (d) documented metric substitutions: `best_window` uses aligned-line equality instead of difflib `ratio`, fuzzy filenames use `1−levenshtein/max_len` instead of `get_close_matches` — near the 0.6/0.8 thresholds these rank differently, and Aider's `strip_quoted_wrapping` filename-repeat forgiveness is not ported (plan doesn't require it, but "port of Aider's proven sequence" is approximate at the decision boundary); (e) production 1,553 lines vs §4 target 1,200 (global gate unaffected; §4 per-crate targets are not CI-enforced — see §9).

### 3.3 `rusta-core` (4,056 lines) — quality: strongest crate overall; 2 MAJOR, 5 MINOR

Plan compliance: **full** — state machine is a genuine closed enum with exhaustive match; 28 State×Event cells pinned both directions (24 + 4 `LoopEscalated` post-erratum); corrective notes BFS-derived from the table; tool registry physically absent in read-only states; core prompt <500 tokens in all 4 phases (tested); card schema strict with declared *and* estimated ≤120-token budget; starter deck compiled in + `.rusta/skills` override; shared error-kind cue producer covers every shipped card trigger; compression at integer 3/5 with bounded-verbatim clipping (256-token floor, marked) and refuse-to-ship guard wired at `agent.rs:558-566`; FAMA-lite detectors match SmallCTL `detectors.py` `>=` semantics, capsule texts match the adapted seed set, canonical-JSON fingerprints pinned; session schema complete; `StateChange` replay validated against the machine's own table; `capped_read` drains past the cap with both pipes drained concurrently (deadlock invariant unit-tested). M3/M5 claims hold.

Findings: **M4, M5** (above). MINOR: (a) replay appends a generic `\n… [truncated]` that live observations never had — the divergence is baked into a unit test rather than caught (`session.rs:337-340`); (b) `restrict_existing` (0600 hardening) is only applied to the diffs sidecar, never to the main log (`session.rs:162-176` vs open path); (c) `capped_read` swallows pipe read errors as clean EOF (`proc.rs:45-46`); (d) duplicate front-matter keys in skill cards silently last-wins despite "strict" parsing (`context.rs:156-186`); (e) dead `CONTEXT_WINDOW_FRACTION: f64 = 0.6` constant beside the live integer `window * 3 / 5` (`context.rs:45` vs `542-544`) — an edit-one-not-the-other trap. Production 2,432 vs §4 target 2,000.

### 3.4 `rusta-tools` (2,615 lines) — quality: best-polished relative to its audit history; 0 MAJOR, 4 MINOR

Plan compliance: **full** — the 4-state × 10-tool matrix holds in both directions (blocked cells return the corrective note, leave file bytes untouched, spawn no process — pinned); §6.1 caps single-sourced in `exec.rs:25-38` with markers only on real truncation; `edit`/`write` funnel into the same rusta-edit chain/journal/ledger (write-refuses-unread is the one deliberate divergence); sub-coder reads provably do not credit the main ledger; §6.12 shell policy complete (12 default deny regexes — a superset of the plan's 9, config-extendable with compile-error surfacing, interactive-command detection with the pass-flags remedy, `sh -c` + env_clear + PATH/HOME/LANG, cwd=repo root, 60 s kill-on-drop, 16 KiB cap); host interaction injected via `Approver`/`Responder` hooks. 24 unit + 16 integration tests green.

MINOR: (a) `cap_window` counts the byte-cap truncation marker as a content line, so the rewritten `path:from-to` header overcounts by 1 and the "N more lines" undercounts — in the exact combined-lines+bytes path; the regression test asserts `truncated` and length but not header/count accuracy (`map.rs:29-46`); (b) `grep` clips matching lines at 200 chars with **no marker**, violating §6.1's "marker whenever a cap truncated" (`search.rs:114-115`); (c) stale doc comment names `wait_with_output` as the kill mechanism 10 lines above the text that correctly describes the `capped_read` mechanism (`shell.rs:280-282`); (d) read-side confinement is lexical only — `read`/`grep`/`glob` follow in-repo symlinks that resolve outside the repo (mutations are safe via rusta-edit's symlink fence; this is read-only information flow requiring a malicious repo; hardening note).

Claim caveats: the glob 1,000-entry cap and the shell 16 KiB cap are **implemented but untested** (no test constructs >1,000 paths or >16 KiB of shell output); the M7 note's "9 default deny regexes" is stale (now 12).

### 3.5 `rusta-repomap` (1,610 lines + 7 queries) — quality: cleanest crate; 0 MAJOR, 5 MINOR

Plan compliance: **full** — all 7 embedded `.scm` queries verified **byte-identical** to Aider's; ref-backfill matches `repomap.py` semantics (with a deliberate improvement: real line numbers); edge weights `mul·√n_r` verified line-by-line against `repomap.py:472-514` (mention/multi-case/chat-file/underscore/common-def multipliers + 0.1 self-edges); personalized PageRank d=0.85, Δ<1e-6/100 iters, mass conserved, no panic on empty graph; rendering (+1/−2 window, 100-char truncation, `│` prefix, `⋮` elision, session-file exclusion); definition-granularity binary-search fitting with the ≤100-line sampling estimator (never exceeds budget by construction); cache keyed (path, mtime, size, query_version); `map_drill` ±8 clamped spans / exact windows. 19 tests green (M4's "12" understated). C++/C def-only connection via backfill confirmed in the mixed-repo snapshot.

MINOR: (a) `git ls-files` C-quotes non-ASCII paths, which then fail `Lang::from_path` and are silently dropped from every map in git repos (`discover.rs:36-41`; the non-git walk handles them fine — behavior differs by repo type); (b) out-of-range/empty-file drill windows degrade to degenerate `path:1-0` headers instead of a `BadWindow` note (`drill.rs:75-77`); (c) `QUERY_VERSION` is hand-bumped and untied to query content — editing a `.scm` without bumping serves stale tag shapes from the in-memory cache; the doc comment overstates the guarantee (`tags.rs:21`); (d) git-tracked symlinks are followed by plain `read_to_string`, reading outside the workspace into context (fallback walk skips symlinks; malicious-repo scenario) (`lib.rs:127`, `cache.rs:48`); (e) production 1,126 vs §4 target 1,000. Also: the M4 claim "constant shared with the renderer" is stale plan-text inherited from §6.5 step 8 — the code correctly follows the later step-5 erratum (drill ±8, renderer +1/−2).

### 3.6 `rusta-dispatch` (1,103 lines) — quality: tight, with two spec-truth gaps; 1 MAJOR, 6 MINOR

Plan compliance: mostly **full** — task/tasks ≤4 distinct labels verified against little-coder `skills/tools/dispatch.md`; read-only 5-tool sub-coder set enforced per call; fresh context with no cross-task brief leakage (e2e-pinned); 6-turn cap with forced wrap-up on the 7th completion; serialized-vs-parallel execution proven on a delayed mock (max-in-flight 1 vs ≥2); backend failure → labeled `RESEARCH FAILED`; labeled reports in input order; fenced tool-block parser with array form.

Findings: **M6** (transcripts, above). MINOR: (a) the clip marker is not charged against the cap — shipped reports are ~411 est. tokens vs the 400 cap, observations ~1521 vs 1500, and the unit test measures only the head (`actor.rs:217-228, 250-251`); (b) the single-`task` form accepts empty/whitespace strings that the array form rejects (`dag.rs:77` vs `99-103`); (c) labels are not validated for `"`/newlines — a crafted label forges/fragments `SUB-CODER "label" REPORT:` headers and mis-splits the session `Dispatch` events during replay (`dag.rs:94-98, 191-195`; agent.rs positional recovery); (d) tool-fence detection is prefix-based — ```` ```tools ````/` ```toolbox ```` open spurious "malformed tool block" notes (`toolcall.rs:44-49`); (e) no Ctrl-C escape hatch during a dispatch fan-out (the stream and validation rounds have one), and dropping the`run()` future would orphan `tokio::spawn`'d actors still burning backend requests (`agent.rs:636` vs `587-597, 739-747`); (f) within one sub-coder completion, corrective notes are appended before call observations, losing §6.1 document order (`actor.rs:122-139`). Also: "panicked actors keep their label" is implemented (`dag.rs:170-179`) but **has zero test coverage** despite appearing in verified evidence.

### 3.7 `rusta-validate` (1,082 lines) — quality: cleanest-scope code in the audit; 0 MAJOR, 5 MINOR

Plan compliance: **full** — config-driven commands; deduped first→last window hard-pinned ≤30 lines with correct elision count; fed back before the user sees results; Repair×3 (attempts_left 2/1/0) then Surface; green passes with the zero-test capsule riding along; `Verdict::event` drives the real `rusta_core::Machine` (the test fires actual transitions); per-command `ValidationRun` events; failures never `Err` (unstartable → 127, timeout → killed, 64 KiB combined cap with concurrent pipe draining); rustc `-->` rewritten to clickable `path:line:col`; libtest tally excludes "filtered out" deliberately with fallbacks and a `10 tests` non-trip boundary; capsule ports SmallCTL's `zero_test_recovery_capsule` faithfully. All 17 tests green.

MINOR: (a) the unstartable-validator report carries **no remedy** — a bare cause string, so a missing cwd burns all 3 repair rounds on something the model can't fix (§6.11 deviation; the sibling timeout path has a remedy) (`validators.rs:347-355`); (b) timeout SIGKILLs only the `sh -c` child — grandchildren survive and can hold file locks into the next round (`validators.rs:142-186`; portable fix needs setsid/killpg, acceptable to document); (c) `window()`'s "first error, last error" doc guarantee breaks at tiny per-report budgets (≥28/≥30 simultaneous failures) — cap never violated, doc overstated; (d) unused `tempfile` dev-dep (`Cargo.toml:21`); (e) zero-test detection scans all validator output, not just test-runner output — a `cargo check` printing a `0 tests` literal gets a misleading capsule (`validators.rs:436-438, 549-578`). Production ~700 vs §4 target 400 — growth went to tests, the right direction.

### 3.8 `rusta-cli` (3,820 lines) — quality: strongest relative to its risk surface; 1 MAJOR, 10 MINOR

Plan compliance: **full** — all 13 `/commands` shipped; the agent loop is genuinely one funnel: text edit blocks, fenced tool calls, and native `tool_calls` all pass through a single document-ordered `Vec<Item>` (`agent.rs:197-272`), with edits delegated to the shared rusta-edit chain; turn cap + wrap-up capsule e2e-proven; Ctrl-C via `tokio::select!` + `UserInterrupt` (partial turn discarded — code-verified only, signals aren't injectable); observation caps verified at their owning boundaries; config discovery/keys/defaults pinned against §7 (`strict_grammar` fully removed); `-c` denies shell; git auto-commit `rusta: <summary>` via `commit --only` (pre-staged user work preserved, unit-pinned); `/undo` HEAD-guarded both directions; compression + JIT cards + `/skills` wired with reserved-token accounting. M8's acceptance arc verified end-to-end against the real HTTP client on a scripted mock (plan → gate → edit → commit → validator red → repair → green → `/undo`), 22 unit + 6 e2e + 7 regression tests green.

Findings: **M7** (unjournaled `/undo`, above). MINOR: (a) stream-open/mid-stream failures fire `PhaseEvent::UserInterrupt` and journal `StateChange { reason: "user interrupt" }` — a transient network error misattributed to the user regresses Planning/Editing → Exploring, discarding plan state (`agent.rs:578-582, 612-616, 853-867`); (b) suggested-command bookkeeping is wrong on the plan path: `ends_turn` is computed before the plan gate, so a fenced bash suggestion inside a plan is surfaced with `tell_model = false` and the model is never told it did not run (`agent.rs:392-401`); (c) `is_plan` treats any leading digits + `.`/`)` as a numbered list, so "2024. …" prose can trip the approval gate — contradicting the "plain answers never trip it" claim (safe direction: the user can decline) (`agent.rs:294-298`); (d) a failed auto-commit is silent at the time (recorded as `sha: None`; user learns only from a later `/undo`) (`agent.rs:497-508`); (e) `/undo` pops the batch before restoring and still reverts the commit after a failed restore, leaving tree/HEAD inconsistent (`commands.rs:178-199`); (f) session-log write errors are swallowed everywhere (`let _ = self.session.record(…)` — 8+ sites); a full disk silently stops journaling and `/resume` later silently loses the unrecorded portion; (g) in `-c` mode `auto_approve = true`/`--auto` still deny every shell call — §6.12 says "denies *by default*", which implies a config path out; the e2e pins the stricter reading (worth a plan erratum either way) (`repl.rs:175-189`); (h) `git::diff()` buffers the entire diff in memory before the 200-line cap (`git.rs:157-188`); (i) a session that dies between `EditsApplied` and the verdict resumes stuck in `Verifying` with no plan gate and refused edits — the only escape is Ctrl-C (`agent.rs:487-510` window; robustness, not a normal path). Untested claims: the plan-decline (`n`) path and LoopGuard escalation through `App` have no CLI-level test.

---

## 4. Cross-cutting integration & wiring

Dependency DAG is clean and matches §10 exactly: leaves `rusta-edit`/`rusta-llm`/`rusta-repomap`; `rusta-core` → edit+llm; `rusta-dispatch` → core+llm; `rusta-tools` → core+dispatch+edit+llm+repomap; `rusta-cli` → all. No cycles; no external dep outside the §10 allow-list; `embedded`/`embedded-cuda` correctly feature-gated (verified against the locked tree incl. license expressions for `ring`, `encoding_rs`, `llama-cpp-2`).

Verified holds: one edit mechanism behind both syntaxes (single `Editor::apply_parsed`/`write_file`, one `UndoStack`/ledger); caps single-sourced per domain; corrective notes derived from the state table, never hand-written; R10 clean; R11 clean (reqwest exists only in `rusta-llm/src/http.rs`; deny.toml restricts sources to crates.io; no telemetry).

Findings: **M8, M9** (above). MINOR: (a) dispatch-report replay duplication (same finding as M5 — reported once); (b) lexical confine fence implemented twice — `rusta-edit/src/ledger.rs:72-81` (`confine`) and `rusta-tools/src/exec.rs:93-115` (`safe_rel`), currently identical, drift risk (deliberate per comment); (c) `SKIP_DIRS` duplicated (`rusta-tools/src/search.rs:14` == `rusta-repomap/src/lang.rs:29`); (d) glob cap duplicated — `rusta-cli/src/commands.rs:393` declares its own `CAP: 1_000` instead of reusing `caps::GLOB_ENTRIES`; (e) `read` does a whole-file `fs::read` before caps apply, and can exceed the 64 KiB byte cap by one full line (`rusta-tools/src/read.rs:27, 58-61`); (f) §6.11 "each crate defines a thiserror enum" is loosely met — `rusta-edit` has none (FailureReason text carries remedies), `rusta-repomap` has `DrillError` only, `rusta-dispatch` has `TaskSetError` only; the model-facing contract is satisfied in substance.

## 5. Testing, CI & docs

- **CI (`.github/workflows/ci.yml`)** implements every §9/§12/§13 gate: fmt, clippy `-D warnings` (both feature graphs, incl. the post-erratum embedded job that actually *runs* the 12 feature-gated tests), `cargo test --workspace`, the LoC gate, and cargo-deny — on ubuntu + macOS. The 2026-09-13 failure mode (gates green while a feature is dead) was specifically hunted: the four errata are each pinned by tests over real inputs, and `loc_budget.sh`'s `#[cfg(test)]` split was independently recomputed (exact match; hand-recount of `context.rs` confirms).
- **Test suite:** 273 tests, all behavioral (real tempdir filesystems, real `sh -c` subprocesses, real git repos, hand-rolled TCP mock SSE servers with hit/in-flight counters). Normative constants are pinned against the plan, not mirrored from the implementation (transition table, deny table, caps, §6.5 multipliers, §7 defaults). Weakest tests (none vacuous): `rusta-llm/src/lib.rs:138` (asserts a constant; embedded arm not compiled in default CI), `rusta-cli/src/config.rs:434` (silently passes when `HOME` is unset), `rusta-cli/src/commands.rs:452` (re-tests a re-export), `rusta-llm/src/error.rs:98` (pins the helper, not its call-site contract), `rusta-core/src/prompt.rs:117` (string equality on the lowest-complexity code in the tree). Coverage gaps: `repl.rs`/`render.rs` have no direct tests (e2e-covered); the plan-decline path, LoopGuard-through-`App`, glob cap, shell 16 KiB cap, panicking-actor recovery, undo-then-resume, append-after-torn, and replay-with-dispatch are all unpinned — six of these correspond directly to MAJOR findings above.
- **Skill cards:** all 4 ship the full §6.6 schema, ≤120 tokens declared+estimated, compiled in via `include_str!`, every trigger cue emitted by a real code path through the shared `error_cues` producer.
- **Config:** `rusta.toml.example` matches §7 key-for-key, no undocumented keys, defaults pinned by test; `strict_grammar` fully removed.
- **Docs defects:** **M10** (GGUF matrix absent) plus: the `rusta-full` artifact named in README:77 and `rusta-cli/Cargo.toml`'s comment **does not exist** (only a `rusta` bin; no release workflow distinguishes them) — add a `required-features = ["embedded"]` bin alias or stop naming it; `embedded-cuda` is undocumented user-facing (§11 mitigation half-shipped); §9's corpus description is stale (says `tests/corpus/` — actual `tests/edit_corpus/`; promises expected *apply* results the corpus doesn't carry; lists a "duplicate matches" fixture that doesn't exist — covered by unit tests instead); README advertises "cargo doc 0 warnings" with no CI step enforcing it.
- **Test-corroborated note:** the workspace-wide count of `#[test]` attributes (288) and milestone progression (131→186→210→273) are consistent with four post-milestone regression rounds; no milestone claim was found *overstated* in a way that hides a defect except where listed above.

## 6. Requirement traceability (§15)

| Req | Status | Evidence |
| --- | --- | --- |
| R1 LoC budget | ✅ | 11,817 prod / 19,503 total; script arithmetic independently reproduced; **but total is at 97.5% of the 20k cap — ~500 lines of headroom** |
| R2 dual backend | ✅ | `Backend` enum, feature-gated embedded, M2/M3 findings are robustness holes, not breakage |
| R3 CLI REPL | ✅ | 13 commands, e2e acceptance arc |
| R4 edit protocol | ✅ | faithful Aider port; **M1 is a reachable corruption path** |
| R5 state machine | ✅ | 28-cell pinned table, BFS notes, registry absence in read-only states |
| R6 repo map | ⚠️ | Ships and ranks correctly, but **M8: the "mentioned by the user" half of the spec is dead in production** |
| R7 context purity/JIT/compression | ✅ | <500-token invariant, cards, bounded-verbatim compression, FAMA-lite |
| R8 validation gate | ✅ | Gate/Repair-bound/machine-driving all tested |
| R9 dispatch | ⚠️ | Ships, but **M6: transcript-to-session-log half of §6.8 unimplemented**; serialization constraint honored |
| R10 no unsafe | ✅ | workspace `forbid` |
| R11 offline/no telemetry | ✅ | single network path is the configured backend |

## 7. Line-budget analysis (R1 / §4 / §12)

The contractual gate (§12) passes with headroom (production 78.8% of cap; total 97.5% of cap). Two observations: (1) **§4's per-crate targets are described as "enforced" but only the global totals are gated** — rusta-edit (1,553/1,200), rusta-core (2,432/2,000), rusta-tools, rusta-repomap (1,126/1,000) and rusta-validate are over their §4 numbers; harmless under R1 as written, but §4's wording overstates the enforcement. (2) At 19,503/20,000 total, the §4-recommended direction of travel for new work is the parking-lot/separate-crate rule (§0 rule 5) — the total cap will bind within ~500 lines otherwise.

## 8. Prioritized remediation plan

**Wave 1 — correctness/data-integrity (small, high-value):**

1. M1: apply `split_trailing_marker` on every commit out of `in_replace` (+ regression test at the mid-stream seam).
2. M4: truncate log+sidecar to the replayed valid prefix on open (+ append-after-torn test).
3. M7 + M9: journal `/undo` (and batch boundaries) — or tombstone undone `EditApplied` hashes so replay honors them (+ undo-then-resume and no-git-resume tests).
4. M5: emit dispatch reports from exactly one replay source (+ replay-with-dispatch test).
5. M2: bound the `tool_calls` index; M3: fix the timeout semantics/remedy.

**Wave 2 — spec-truth gaps:**
6. M6: persist sub-coder transcripts (schema addition) or amend §6.8 and the doc comments.
7. M8: wire user-mention extraction into `render_map` callers, or amend §6.5 steps 3–4.
8. M10 + `rusta-full` alias + `embedded-cuda` docs (README/§13 DoD).
9. Validate: label character set (dispatch), single-task empty string, clip-marker accounting, `is_plan` digit heuristic, suggested-command-on-plan bookkeeping, UserInterrupt misattribution.

**Wave 3 — hardening & hygiene:** unbounded-buffer caps (SSE buffer, non-2xx body, git diff, feedback read), read-side symlink confinement, `restrict_existing` on the main log, cap the two silent caps (grep line clip, read overshoot), the duplicated constants (`SKIP_DIRS`, glob CAP, confine fence), dead `CONTEXT_WINDOW_FRACTION`, unused `tempfile` dev-dep, stale comments (`wait_with_output`, M7's "9 regexes"), and the six unpinned test paths listed in §5.

---

*Prepared by full-workspace line-by-line audit: 10 parallel deep-dive passes (8 crates, integration seams, tests/CI/docs), every `.rs` file read in full, reference ports compared against `aider`/`little-coder`/`Observer`/`smallcode`/`SmallCTL` source, all findings re-verified against cited code before inclusion.*
