# AUDIT REPORT — Rusta (Round 7)

**Project:** Rusta — AI coding-agent harness for small local LLMs (8B–35B)
**Repository:** `vladimirpesic/rusta` · Workspace: 8 crates, edition 2024, Rust 1.85
**Date:** 2026-09-17
**Auditor:** Kimi Code CLI — eight parallel deep-audit passes (one per crate boundary: 7 crates + cross-crate wiring/infrastructure), plus an independently executed gate run and lead re-verification of every high/medium finding against source.
**Scope:** Full line-by-line review of all 49 production files (12,337 LoC) and all 8,603 LoC of tests; wiring/integration across every crate boundary; CI, scripts, configs, docs; semantic-parity comparison against the reference projects in `/home/vladimir/develop/refs/` (Aider, little-coder).

> **History note.** A prior `AUDIT_REPORT.md` (six rounds, 25 findings) was deliberately folded into
> `ADR.md §16` and deleted — the ADR is now the single source of truth. This document is a **new,
> seventh round**. Where it touches the prior 25 findings, it re-verifies them in the current code
> rather than trusting the ADR's claims. Per the ADR's own acceptance rules (§16.1–§16.3), every
> finding below was either executed/reproduced or read at the cited `file:line`; items that could
> not be proven are explicitly quarantined in §7 as *risks*, never as findings.

---

## 1. Executive Summary

**Overall verdict: 4 / 5 — genuinely milestone-complete, with one high-severity confinement regression and a cluster of medium issues concentrated at unfenced paths, untested guards, and documentation drift.**

The claims in `README.md` and `ADR.md §17` reproduce exactly under independent re-execution: fmt clean, `clippy -D warnings` zero on **both** feature graphs, 292 tests passing (304 with `embedded`), `cargo doc` zero warnings, LoC 12,337 production / 20,940 total against the 15k/25k budget. The prior audit rounds' fixes (A1–A25, C/G/L/F classes) were re-verified as actually present in code, several pinned by tests that assert the detector's own proof of life. The Aider ports (edit apply chain, repomap ranking) were compared line-by-line against the reference source and are faithful, with deliberate deviations documented at the point of divergence.

What prevents a clean bill:

1. **One high-severity recurrence of the A10 confinement class** — `grep` has no outside-repo/symlink fence while `read` and `map_drill` do (the §6.12 erratum names all three; two got the fence). Executed: an in-repo symlink leaks outside-repo file contents into model context today. Also reachable from sub-coders.
2. **The §6.12 shell deny table does not implement its own documented 9th rule** ("any write outside the repo root") — `rm -rf ~`, `echo x > ~/f`, `mv f /tmp/../etc/...`-class commands statically pass the gate and reach the approval prompt.
3. **A silent-truncation semantic in the HTTP SSE pump**: a server that closes mid-stream without `finish_reason`/`[DONE]` reports a clean `Finish(Stop)`; the CLI commits the partial text as the turn's answer. Entirely untested path.
4. **`/undo` under mid-batch disk failure destroys the remaining journal entries and still reverts the commit**, leaving tree and history inconsistent with no recovery path.
5. **A systematic "guard says so" residue**: §16.4 claims all 25 prior findings are pinned by regression tests, but the A8 fix has no test; the §6.1 cap-marker rule ("a cap's own marker is charged against the cap") is violated in `clip_observation`, `grep` line clips, and shell output caps, while being honored in the sibling `cap_report`; several model-facing outputs are silently wrong (a fabricated `read` line-range past EOF, a `path:1-0` drill header on empty files) in exactly the way this codebase treats as load-bearing.
6. **Documentation drift in the normative documents** — the same defect class §16 exists to police, here in the docs' favour: ADR §9's corpus path is wrong, `[validate].timeout_secs` exists but is undocumented, `state.rs`'s module header and M3 acceptance doc miscount the table they pin (six/seven events, 24/28 cells), and several doc comments state behaviour the code does not have.

No critical-severity issues were found. Nothing found undermines the architecture; the ADR's own §17 pre-alpha caveats (zero real-model exposure, `/auto`+shell danger) remain the dominant risk and are honestly owned there.

### 1.1 Scorecard

| Crate / Scope | Score | Verified findings (sev) | One-line verdict |
| --- | --- | --- | --- |
| rusta-llm | 4/5 | 6 (0 med+) | Tight dual-backend crate; silent mid-stream truncation + untested A8 guard |
| rusta-edit | 4/5 | 4 (0 med+) | Most faithful Aider port; corpus apply expectations never asserted; CRLF asymmetry |
| rusta-core | 4/5 | 3 (0 med+) | Best-pinned hub; latent capsule-budget gap; two stale doc miscounts in the load-bearing file |
| rusta-repomap | 4/5 | 4 (1 med) | Byte-identical queries, verified PageRank; unfenced render read path |
| rusta-tools | 3.5/5 | 4 (1 high, 1 med) | Honest matrix test, but A10 recurs in `grep`; deny-table gap; fabricated read range |
| rusta-dispatch + rusta-validate | 4/5 | 3 (0 med+) | Unusually honest failure modes; clip marker exceeds its own cap, untested |
| rusta-cli | 4/5 | 4 (1 med) | Most heavily defended crate; `/undo` partial-failure hole; two false user-facing messages |
| Wiring & infrastructure | 4/5 | 4 (0 med+) | Strongest part: clean DAG, total workspace inheritance, exact claim reproduction; 4 doc drifts |

### 1.2 Gate Status (independently re-executed, 2026-09-17)

| Gate | Command | Result |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | **PASS** (exit 0) |
| Clippy, default graph | `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** (exit 0) |
| Clippy, embedded graph | `cargo clippy --workspace --all-targets --features rusta-cli/embedded -- -D warnings` | **PASS** (exit 0) |
| Tests, default graph | `cargo test --workspace` | **PASS** (exit 0) |
| Tests, embedded graph | `cargo test --workspace --features rusta-cli/embedded` | **PASS** (exit 0) |
| Docs | `cargo doc --workspace --no-deps` | **PASS** (0 warnings) |
| LoC budget | `scripts/loc_budget.sh` | production 12,337 / 15,000 · tests 8,603 · total 20,940 / 25,000 — matches ADR §17 to the line |
| Test count | summed `cargo test -p <crate> -- --list` | **292** default / **304** with `embedded` — matches ADR §17 exactly |
| R10 no-unsafe | workspace lint `unsafe_code = "forbid"` | enforced by compiler; only unsafe in project is inside `llama-cpp-2`, behind `embedded`, isolated by the `Backend` boundary |

---

## 2. Consolidated Findings — Verified

Severity: **H** high · **M** medium · **L** low. Findings marked **[lead]** were re-verified independently by the lead auditor against source, not only by the sub-audit. All others carry `file:line` evidence from the sub-audits and were read at those locations during report assembly.

### 2.1 High

| ID | Sev | Location | Finding |
| --- | --- | --- | --- |
| F-01 | **H** | `crates/rusta-tools/src/search.rs:90-108` **[lead]** | **`grep` has no outside-repo/symlink confinement** — an exact recurrence of the A10 class. `read` uses `safe_rel_in` and `map_drill` uses `safe_rel_in`, but `grep` reads every walked file with plain `fs::read(root.join(&rel))`. The §6.12 read-side erratum names `read`, `grep` **and** `map_drill`; two of three received the fence. **Executed by the auditor:** an in-repo symlink `link.txt → /outside/secrets.txt` in a scratch repo; `grep {"pattern":"hunter2"}` returned `Ok` with the outside file's content, while `read` on the same path refused. Also reachable from sub-coder `grep` (sub-coders are advertised as read-only-isolated; this leaks across the isolation boundary). The A10 regression test (`tests/regressions.rs:473`) exercises only `read`. This is precisely the §16.2 acceptance-rule failure: "a change … must be checked against every consumer" was applied to two of three named tools. |

### 2.2 Medium

| ID | Sev | Location | Finding |
| --- | --- | --- | --- |
| F-02 | **M** | `crates/rusta-tools/src/shell.rs:64-129` **[lead]** | **The deny table does not implement §6.12's stated 9th default, "any write outside the repo root."** The table (12 entries) covers: `rm` targeting `/`, sudo, force-push, dd, mkfs, fork bombs, shutdown family, curl\|sh, absolute-path redirects/tee (`>` `/…`), `../` redirects/tee, cp/mv/ln to a *listed* system dir, and `cd` out. It does **not** catch writes via `$HOME`/`~` (`echo x > ~/f`, `tee ~/out` pass), `rm -rf ~` / `rm -rf $HOME`, or path-traversal spellings that don't start with a listed dir (`mv f /tmp/../etc/cron.d/x`). Verified by executing `ShellPolicy::check` on each of those — all returned `Verdict::Allowed`. Mitigations exist (approval gate; `[shell].allow` prefixes bypass it, `/auto` skips it) but the static gate's documented contract is broader than its implementation. |
| F-03 | **M** | `crates/rusta-llm/src/http.rs:429-431` **[lead]** | **Silent truncation on mid-stream disconnect.** A server that closes the connection without `finish_reason` or `[DONE]` is reported as a clean `Finish(Stop)`. `pump_events` ends when the byte stream ends and emits `Finish(finish.unwrap_or(FinishReason::Stop))`; there is no `[DONE]`-seen tracking. The only consumer (`crates/rusta-cli/src/agent.rs:641`) breaks on `Finish` and commits the partial text as the turn's answer. Truncated model output is indistinguishable from a complete answer — and the path is never exercised by any test (see F-05). |
| F-04 | **M** | `crates/rusta-cli/src/commands.rs:186-214` + `crates/rusta-edit/src/apply.rs:125-139` **[lead]** | **A partial `/undo` disk failure loses the rest of the batch permanently and still reverts the commit.** The batch is popped before restoring (`commands.rs:187`); `UndoStack::undo_last` pops the journal entry *before* the fenced write (`apply.rs:126` pop, `apply.rs:138` `guarded(...)?` — the `?` returns after the pop). On `Err` the loop breaks with entries destroyed, then execution unconditionally proceeds to `reset_if_head` (`commands.rs:214`) regardless of how many entries were restored. Result: tree and history inconsistent, and no retry is possible (the next `/undo` reports "nothing to undo"). |
| F-05 | **M** | (coverage) `crates/rusta-llm` | **No test ever exercises the `StreamEvent::Failed` path or the disconnect-without-`[DONE]` semantics** — across all three test files (mock_server.rs, http_e2e.rs, embedded_e2e.rs), no step produces a mid-stream failure or a truncated stream. F-03's behaviour and the A8 `Malformed` path both ship untested. Reviewed all test files; confirmed by auditor. |
| F-06 | **M** | `crates/rusta-llm/src/http.rs:384-390` vs ADR §16.4 | **The A8 fix (wire `tool_calls` index bounded at 64) has no regression test anywhere in the workspace**, contradicting ADR §16.4's claim "All 25 findings were resolved and each is pinned by a regression test." Grep for out-of-range index patterns finds only the implementation and its comment. The implementation itself is correct (index ≥ 64 → `Error::Malformed`, with a clear attack comment); the guard is never proven looking. |

### 2.3 Low (verified)

| ID | Sev | Location | Finding |
| --- | --- | --- | --- |
| F-07 | L | `crates/rusta-repomap/src/lib.rs:127` (+ `cache.rs:48`, `discover.rs:58`) **[lead — code read; path reproduced by auditor]** | **Map render path follows symlinks out of the repo.** `render_map`'s read closure is a plain `fs::read_to_string(root.join(rel))` with no `contains_path` check; `git ls-files` lists tracked symlinks and the walk pushes them too. Auditor reproduced: a git-tracked `src/linked.rs` symlink pointing outside the repo rendered the outside file's content into the map under a repo-relative header. The same path via `map_drill` is blocked by `safe_rel_in` — the fence is inconsistent between the two map views. Same vulnerability class as A10/F-01, unfenced here. |
| F-08 | L | `crates/rusta-repomap/src/discover.rs:37-40` | **Non-ASCII filenames are silently dropped in git repos.** `git ls-files` C-quotes non-ASCII paths (`core.quotepath` defaults on); rusta takes output literally, the quoted path never matches a real file, `cache.rs:41` returns `None`, and the file vanishes from the map with no warning. The walk fallback handles such names fine, so git vs non-git behavior diverges. Reproduced with `src/café.rs`. One-flag fix (`-c core.quotepath=false` + `-z`) or quote-unescaping. |
| F-09 | L | `crates/rusta-repomap/src/drill.rs:75-77` | **`Window` on an empty file yields header `path:1-0`** — `from` clamps to `lines.len().max(1)` = 1 while `to` clamps to 0, a contradictory 1-based range naming a line that doesn't exist (reproduced). Violates §6.5 step 8's header contract; the model receives a nonsense range. |
| F-10 | L | `crates/rusta-repomap/src/lang.rs:70` | **Factually false parity claim**: "TSX shares TypeScript's tags query (Aider does the same)." Aider has *no* `tsx-tags.scm` in either query pack and yields zero tags for `.tsx` (`repomap.py:805-831, 292-294`). Rusta's choice (reuse the TS query) is a reasonable superset and violates no ADR text, but the cited justification is false. |
| F-11 | L | `crates/rusta-tools/src/read.rs:47,104-111` **[lead]** | **A `read` window starting past EOF fabricates a line range.** `last_line` is initialised from the *unclamped* `from` and never clamped to `total_lines` when nothing is shown. Executed: 3-line file, `read(a.rs, from:10, to:12)` → `Ok`, not truncated, content `"a.rs:3-9\n"` — header claims lines 3–9 of a 3-line file, empty body, no remedy. Misleading in exactly the way the codebase treats as load-bearing (a SEARCH anchored on wrong beliefs). |
| F-12 | L | `crates/rusta-tools/src/shell.rs:301-303` | Stale comment: "the timeout's drop of the `wait_with_output` future" — the code no longer uses `wait_with_output`; it builds a custom `collect` future (`shell.rs:349-355`). Comment describes the removed implementation. |
| F-13 | L | `crates/rusta-dispatch/src/actor.rs:168-177` **[lead]** | **`clip_observation` does not charge its clip marker against the cap**, contradicting §6.1 ("a cap's own marker is charged against the cap, never added on top") and the sibling fix in `cap_report` (`actor.rs:222-227`, which subtracts the marker first — a comment there records the ~411-token bug this prevents). Head of 4,500 chars + 61-char marker = 4,561 chars → 1,521 estimated tokens against a 1,500 cap. Also has zero test coverage (F-14). |
| F-14 | L | `crates/rusta-dispatch/src/actor.rs:168-177` | `clip_observation` has zero test coverage — no test in actor.rs/dag.rs/e2e.rs exercises the 1,500-token observation clip; precisely where F-13 lives. Same "detector must prove it is looking" shape. |
| F-15 | L | `crates/rusta-dispatch/src/dag.rs:77` vs `dag.rs:99-103` | Input-validation asymmetry: the single-task form (`{"task": ""}` / whitespace-only) bypasses the non-empty validation the array form enforces, spawning a sub-coder with an empty brief. |
| F-16 | L | `crates/rusta-core/src/context.rs:1122-1126` **[lead]** | `capsule_note`'s token-budget guard is conditional on `included > 0`: a *first* capsule whose text alone exceeds `CAPSULE_TOKEN_BUDGET` (180) ships over budget — the ≤180-token §6.6 guarantee has a hole. **Latent only**: `CAPSULES` is a private static whose longest text is ~25 tokens, so the note is ≤~110 today; the regression test exercises only the shipped set. |
| F-17 | L | `crates/rusta-core/src/state.rs:3` | Stale module doc: "Four states, **six** scaffold events, ten canonical tools" — the enum has had seven events since `LoopEscalated` was added (`PHASE_EVENTS: [PhaseEvent; 7]` at state.rs:232). Miscounts the load-bearing table in the file whose contract is exactness. |
| F-18 | L | `crates/rusta-core/src/state.rs:525-527` | Stale test doc on the M3 acceptance test: "all **24** cells … all **sixteen** illegal combinations" — the table pins 28 cells with 18 illegal ones (24/16 was the pre-`LoopEscalated` count). Table and assertions are correct; only the prose miscounts. Together with F-17: the two most load-bearing locations in `state.rs` both miscount the table they exist to pin. |
| F-19 | L | `crates/rusta-llm/src/http.rs:335-339` vs `error.rs:62-73` | Doc claims mid-stream `StreamEvent::Failed` carries "a remedy text (ADR §6.11)", but the two errors that actually reach it — `Malformed` and `StreamFailed` — carry none. The text is user-facing (agent.rs:642-648), so §6.11's model-facing rule is not strictly breached, but the doc comment's claim is false. |
| F-20 | L | `crates/rusta-llm/src/http.rs:450-462, 366-369` | An SSE event consisting of an empty `data:` line (`data:\n\n`, a legal keep-alive variant) becomes `Some("")`, fails `serde_json::from_str("")`, and kills an otherwise healthy stream with `Error::Malformed`. |
| F-21 | L | `crates/rusta-llm/src/embedded.rs:77-82` | `ctx_size = Some(0)` is silently clamped to a 1-token context (producing a confusing runtime overflow error later) instead of a config error, against §6.11's actionable-error posture; `Error::Config` exists but is unused for this. Deliberate (pinned by `ctx_defaults_to_trained_and_clamps`) but un-diagnosed. The CLI happens to guard it (repl.rs:108 maps 0 → `None`). |
| F-22 | L | `crates/rusta-llm/src/http.rs:35-39` | Comment mischaracterizes reqwest `read_timeout` as a cap on *starting* the response; it is a per-chunk inactivity timeout over the whole stream, so any inter-token gap > 120 s kills a healthy slow generation. Defensible choice; comment understates it. |
| F-23 | L | `crates/rusta-edit/tests/corpus.rs:155-195` vs ADR §9 | ADR §9 states each of the 15 corpus fixtures carries "an expected parse **and** an expected apply result"; the corpus test only ever calls `parse_response`. **No corpus fixture is ever run through `Editor`/`apply_parsed`** — the apply expectations the ADR describes are not exercised against the fixtures. Apply paths are covered only by hand-written unit fixtures. §9 divergence. |
| F-24 | L | `crates/rusta-edit/src/apply.rs:631-638` + `crates/rusta-tools/src/edit.rs:22-26` | CRLF forgiveness is asymmetrical between the two edit syntaxes §6.4 requires to be "identical". Text blocks are CRLF-normalized (`parser.rs:113-118`; file content at `apply.rs:585-591`), but `prep()` does not normalize `\r` in SEARCH/REPLACE and the tool-call `edit` path passes `search`/`replace` verbatim — a model emitting `\r\n` inside a tool-call `search` can never match the normalized file. No test covers CRLF via the tool-call form. |
| F-25 | L | `crates/rusta-edit/src/apply.rs:450` vs `597-601` | `AppliedBlock.appended`/`created` are computed from `block.is_new_file()` on the *raw* SEARCH while the apply decision uses the *fence-stripped* SEARCH — a SEARCH of only a wrapping fence pair strips to empty (append/create path) but reports without `(appended)` and escapes `try_cross_file`'s new-file exclusion. Only observable effect is the mislabelled report flag. |
| F-26 | L | `crates/rusta-edit/src/apply.rs:547-553` vs `editblock_coder.py:588-591` | Fuzzy filename resolution returns the *first* read-set file (sorted order) ≥ 0.8 similarity; Aider's `get_close_matches(n=1, cutoff=0.8)` returns the single *best* match. With two read-set files both ≥ 0.8, Rusta can bind an edit to the lexicographically-first, worse-matching file. Unlabelled reference divergence (the Levenshtein stand-in itself is documented; the first-vs-best selection difference is not). |
| F-27 | L | `crates/rusta-cli/src/git.rs:131-140` + `commands.rs:218-222` | When HEAD *is* the recorded rusta sha but `git reset --mixed HEAD~1` fails (root commit has no parent; index lock), `/undo` prints "commit kept — HEAD moved on after it" — a **false explanation**; HEAD did not move, the reset failed. Misleading user-facing text on a real edge (first-ever commit in a fresh repo is a rusta batch in the M8 e2e flow). |
| F-28 | L | `crates/rusta-cli/src/agent.rs:244` | Fence opener matched by prefix: `trimmed.starts_with("```tool")` also matches ```` ```toolbox ```` / ```` ```tools ````. Such a fence is consumed as a tool fence; its body becomes a spurious "malformed tool block" note instead of prose, and any SEARCH/REPLACE content in it never reaches the edit parser. No word-boundary check anywhere. |
| F-29 | L | `crates/rusta-cli/src/config.rs:240-250` | `SessionStart`'s journaled `config` line reports the **file's** `[backend].kind`, ignoring the `--backend` CLI override, while the adjacent `backend` field honors it — the same event can journal `backend=http` next to `backend: embedded ~/models/…`. Replay-time inconsistency in the session log. |
| F-30 | L | `ADR.md:735` vs `tests/edit_corpus/` | ADR §9 locates the parser corpus at `crates/rusta-edit/tests/edit_corpus/`; it actually lives at workspace root `tests/edit_corpus/` (15 `.md` files). Loaders point at the real path; the normative document doesn't. |
| F-31 | L | `crates/rusta-validate/src/validators.rs:84` vs `ADR.md:705-706` + `rusta.toml.example` | `[validate].timeout_secs` is a real, parsed, validated config key but is absent from the normative §7 schema and from the example config. Functional but undocumented — the inverse of the `strict_grammar` archetype. |
| F-32 | L | `scripts/loc_budget.sh:30,41` | The §12 gate scans only `crates/`. Any `.rs` under workspace-root `tests/` (or a future root `benches/`/`examples/`) escapes R1 counting entirely. Harmless today (root `tests/` is `.md`-only) but §12's contract says "counted on `*.rs`" without a `crates/` boundary. |
| F-33 | L | `ADR.md:140` vs root `Cargo.toml` | §5's layout comment describes the root manifest as "features: embedded, embedded-cuda (opt-in)"; the root manifest defines no features — they live on rusta-llm/rusta-dispatch/rusta-cli. Holdover the §13 `rusta-full` revision missed. |
| F-34 | L | `.github/workflows/ci.yml` | **No `cargo doc` step**, though §17 reports "cargo doc 0 warnings" as a gate and ~650 doc comments cite ADR sections; nothing prevents doc-warning drift in CI. (Independently confirmed: doc currently passes, but CI doesn't enforce it.) |
| F-35 | L | `deny.toml:26` | `multiple-versions = "warn"` — a dependency duplication would warn but never fail CI. Currently moot (lockfile has no duplicate name+version pairs, verified by scan). |

---

## 3. Risks & Observations (not proven — quarantined per §16.1)

These were reasoned about but not reproduced; they are recorded so they are investigated, not fixed-by-assumption.

| ID | Sev | Location | Risk |
| --- | --- | --- | --- |
| R-01 | M | `rusta-llm/src/http.rs:346-357` | No cap on the SSE buffer: a server that never terminates an event makes `buffer` grow unboundedly; `buffer.retain(...)` per chunk is O(buffer) per read (quadratic on pathological streams). |
| R-02 | M | `rusta-tools/src/search.rs:27-51` | `walk()` builds an unbounded `Vec` of every repo file (used by `grep`, `glob`, CLI `/add`); §6.1 caps bound the observation, not walk memory. |
| R-03 | M | `rusta-tools/src/map.rs:186` + `rusta-repomap/src/drill.rs:72` | `map_drill` caps the observation but `drill` does `fs::read_to_string` of the *whole file* first — memory ∝ file size, unbounded (contrast `read`, which streams). |
| R-04 | M | `rusta-tools/src/exec.rs:330-351` (+ `rusta-repomap` via `exec.rs:343`) | `read`/`grep`/`glob`/`map_refresh` are blocking sync I/O on the Tokio worker (no `spawn_blocking`); a huge-repo grep blocks a worker and can't be cancelled mid-walk. `render_map` also re-reads every file O(log n) times during budget fitting (no Aider-style tree_cache). |
| R-05 | M | `rusta-dispatch/src/dag.rs:160-183` | `run()` spawns detached tasks; dropping the `run` future does not cancel actors — orphaned sub-coders would keep consuming the backend (and the serialized gate) until completion. Unreachable in the CLI today (no `select!` around dispatch, agent.rs:438); a future caller with a timeout would hit it. |
| R-06 | M | `rusta-cli/src/agent.rs:374-398` | Turn cap is `turn > max_turns`, so a request runs **max_turns + 1** model completions (the 17th streams the wrap-up). Arguably a strict reading of §6.1's "hard cap: 16 turns" — but deliberate and pinned by `turn_cap_forces_a_wrap_up…`. Spec-interpretation note, not a bug. |
| R-07 | M | `rusta-cli/src/agent.rs:56-67, 92-154` | Ctrl-C semantics at blocking interactive prompts (plan y/n, shell y/n/a, ask) unverified and possibly inconsistent: before the first streamed turn, SIGINT default-terminates; once tokio's ctrl-c handler is installed, a SIGINT with no listener pending may be dropped silently. No test covers it. |
| R-08 | M | `.github/workflows/ci.yml:40-72` | The embedded feature graph is CI'd on Ubuntu only; §13's "Linux and macOS, with and without embedded" is verified for macOS only on the default graph (llama.cpp on macOS is a distinct build path). |
| R-09 | L | `rusta-core/src/session.rs:617` | `repair_torn_tail` computes truncation as Σ(line.len()+1), assuming LF endings exactly as Rusta writes them. A CRLF hand-edited log undercounts by one byte per line → repair truncates mid-line. Unreachable with Rusta-written logs. |
| R-10 | L | `rusta-core/src/context.rs:781-786` | `summarize_turns` prepends carried-over prior-summary lines without charging them against `SUMMARY_MAX_LINES` (30); successive compressions could grow the summary past the cap. Deterministic and bounded per compression. |
| R-11 | L | `rusta-core/src/context.rs:655-669` | `apply_summary` with `covers_turns` greater than the assistant-message count replaces the *entire* history, dropping the last-3-turns-verbatim guarantee. Only reachable from a hand-edited/foreign `Summary` event. |
| R-12 | L | `rusta-core/src/context.rs:434` | A skill-card file that fails *reading* (permissions) maps to `Error::Session` with no path, unlike parse errors which carry `Error::SkillCard { path, cause }`. Misleading, path-less remedy. |
| R-13 | L | `rusta-core/src/context.rs:1186-1190` | `error_cues("validation", …)` fires on any "test result:" substring including green `test result: ok`. Both production call sites gate on `Status::Error` today; any future caller passing green output would mis-trigger. Caller-dependent safeguard, the §16.1 shape. |
| R-14 | L | `rusta-core/src/context.rs:566-600` | `compress` never re-checks the budget after inserting summary + keepers; a large carried summary can leave history over the 60% threshold (the CLI's absolute window check still catches total overflow). |
| R-15 | info | `rusta-core/src/session.rs:264-273` | Events are flushed but never `fsync`ed; "a crash loses at most the event in flight" is slightly optimistic (OS buffer loss possible). Standard for this class of tool. |
| R-16 | L | `rusta-llm/src/embedded.rs:262` | Cancel-flag race: `worker_loop` resets `cancel` at job start, so a `stop()` landing between jobs is silently lost. Benign for the Ctrl-C use case. |
| R-17 | L | `rusta-llm/src/http.rs:278-280` | A 404 from a known-good route (llama-server's "model not found") yields the `base_url` hint, misdiagnosing a bad model name as a bad URL. Server behavior not verified. |
| R-18 | L | `rusta-llm/src/http.rs:357` | CR-only SSE line terminators (legal per EventSource spec) collapse into corrupt merged lines → `Malformed`. All realistic servers use LF/CRLF. |
| R-19 | L | `rusta-llm/tests/http_e2e.rs:163-165` | `connection_refused_is_unreachable_after_three_attempts` frees a probe port and hopes nothing rebinds it — a classic rare flake. |
| R-20 | L | `rusta-llm/src/embedded.rs:502-506` | `DeltaEmitter::push` cuts only the first stop matched in list order when one stop is a suffix of another, potentially leaving a dangling prefix. Unrealistic stop configs only. |
| R-21 | L | `rusta-edit/src/apply.rs:243-258` | Glued marker preceded by whitespace leaves the whitespace in committed text (`"new >>>>>>> REPLACE"` → `"new \n"`). Cosmetic; Aider has no equivalent strip. |
| R-22 | L | `rusta-edit/src/apply.rs:578-583, 296-307` | `read_file_utf8`/`write_file` load whole files unbounded — §6.1 caps bind the `read` *tool*, not the edit path. Same whole-file-rewrite design as Aider; design inheritance rather than defect. |
| R-23 | L | `rusta-edit/src/apply.rs:862-893` | `split_on_dot_lines` is strictly line-based; Aider's separator regex lets `\s*` absorb preceding blank lines into the separator piece. Pathological inputs could split differently from the reference (mitigated by the both-sides-must-equal check). |
| R-24 | L | `rusta-edit/src/apply.rs:596-628` vs `editblock_coder.py:351-352` | Aider's `strip_quoted_wrapping` also drops a SEARCH whose first line ends with the target file's basename; not ported → such a block falls to NoMatch + feedback. §0 rule 2 defers to the reference where the ADR is silent. |
| R-25 | L | `rusta-edit/src/apply.rs:988-1019` | `best_window`'s equal-lines ratio (documented difflib deviation) can select a different window than Aider for near-miss searches; the doc comment itself concedes the corpus doesn't exercise near-miss cases. |
| R-26 | L | `rusta-repomap/src/render.rs:57-67` | `fit_map`'s binary search assumes the token estimate is monotone in k; sampling breaks strict monotonicity. Worst effect observed is a smaller-than-optimal prefix — the `<= budget` guard means it can never exceed budget. |
| R-27 | L | `rusta-repomap/src/cache.rs:44` | mtime+size key can serve stale tags on a same-size rewrite within filesystem mtime granularity (same weakness as Aider's cache; ADR documents the cache as best-effort). |
| R-28 | L | `rusta-repomap/src/graph.rs:268-272` vs `repomap.py:543-545` | Tie-break order diverges: equal scores sort by key ascending; Aider sorts ties by key descending. Cosmetic. |
| R-29 | L | `rusta-repomap/src/discover.rs:27-42` | Files deleted from disk but still tracked are silently skipped, while Aider warns the user. No ADR requirement; a map that shrinks without explanation can confuse the model. |
| R-30 | info | `rusta-repomap/src/graph.rs` vs `repomap.py:419-420` | Aider's `if not references: references = defines` fallback is not ported (Rusta uses flat 0.1 self-edges). ADR-compliant divergence — recorded so it isn't "fixed" into a regression. |
| R-31 | L | `rusta-tools/src/{search.rs:116-122, map.rs:29-33, shell.rs:370-377}` | Cap markers appended *past* the cap, not charged against it (measured: clipped grep line 215 chars on a 200 cap; shell content 16,407 bytes on a 16,384 cap). §6.1's marker sentence sits beside the truncation-marker rule; strict reading covers these — needs an ADR ruling (see F-13 for the confirmed instance). |
| R-32 | L | `rusta-tools/src/exec.rs:300-311` | Public `Tools::editor()`/`repomap()` return live `MutexGuard`s while `exec` locks the same mutexes internally — a host holding a guard across `exec` would deadlock. Current CLI call sites hold none. API shape invites the hazard. |
| R-33 | L | `rusta-tools/src/shell.rs:357-361` | On timeout the outcome is `truncated: false` with all partial output discarded — a 60 s test-suite run killed at the cap shows the model nothing. Design choice; "partial output + timeout" might repair better. |
| R-34 | L | `rusta-tools/src/map.rs:104-115` | `mentioned_identifiers` treats any token containing `.` or `/` as pathy, so sentence tokens like `e.g.` become ×10 mention boosts. |
| R-35 | L | `rusta-dispatch/src/actor.rs:36-44` | Doc claim "bounds the transcript to roughly `TURN_CAP × OBSERVATION_CAP`" is false: the bound is per observation, and a completion may carry unbounded tool calls (10 calls × 6 turns × 1500 tokens ≈ 90k ≫ window → whole research fails as `RESEARCH FAILED`; graceful, but the stated bound is false). |
| R-36 | L | `rusta-dispatch/src/actor.rs:146-156` | A wrap-up completion that ignores the instruction and emits tool-call fences is shipped verbatim as the report (no re-parse). |
| R-37 | L | `rusta-dispatch/src/dag.rs:173-179` | Panicked-actor fallback (§6.8 "a panicked actor keeps its label") implemented but untested. |
| R-38 | cosmetic | `rusta-validate/src/validators.rs:293-303` | "N lines elided" count is in deduplicated lines, not raw output lines — a heavily repeating 10k-line build log elides far fewer raw lines than the count suggests. |
| R-39 | cosmetic | `rusta-validate/src/validators.rs:183` | A `child.wait()` error reported as "validator failed to start" with exit 127 — wrong remedy for that near-unreachable case. |
| R-40 | L | `rusta-cli/src/agent.rs:421-440` | Ctrl-C during a running `dispatch` (or any long tool) is not listened for — the signal listener exists only in `stream_turn` and `validate_round`. A 4-task dispatch on a slow backend can't be interrupted short of killing the process. |
| R-41 | L | `rusta-cli/src/git.rs:68-94` | `git add` before `commit --only` leaves batch paths staged post-commit (benign, index = HEAD), but `/undo`'s `reset --mixed HEAD~1` resets the whole index, discarding unrelated user staging performed after the rusta commit (working tree preserved). |
| R-42 | L | `rusta-cli/src/agent.rs:97-99, 120-122, 141-144` | `TerminalGate`/`TerminalApprover`/`TerminalResponder` print via `println!` directly, bypassing `Reporter` — render.rs's "single surface" claim not enforced; tests can't capture prompt text. |
| R-43 | L | `rusta-cli/src/lib.rs:105-108, repl.rs:346-351` | `rusta -c ""` submits an empty user message (no emptiness check in `run_once`). |
| R-44 | L | `rusta-cli/src/lib.rs:82-83, commands.rs` | With no `HOME`, the session log falls back to `<repo>/.rusta-session.jsonl` — a whole-file-content log inside the user's repo, liable to be `git add`-ed. Also: piped stdin to the REPL is silently ignored (verified empirically: `echo hi | rusta` prints the banner and exits 0 without processing input). |
| R-45 | L | `rusta-cli/src/repl.rs:443-448` | A `Commit` event with no preceding confined `EditApplied` (partial journal failure) attaches the new sha to the **previous** request's batch; `/undo` would then reset a commit whose content the batch doesn't describe. Requires journal-write failure; tombstone bookkeeping keeps replay self-consistent. |
| R-46 | L | workspace seams (`rusta-tools/src/shell.rs:364`, `rusta-cli/src/git.rs:186`) | Subprocess output decoded via `String::from_utf8_lossy` — invalid UTF-8 becomes U+FFFD silently at two seams; a lossy conversion the ADR never mentions. |
| R-47 | L | `crates/rusta-core/Cargo.toml:18-20` | `[dev-dependencies]` redundantly re-declares `rusta-edit`/`rusta-llm` already in `[dependencies]` (harmless; only the tokio feature additions matter). |

---

## 4. Per-Crate Reports

### 4.1 rusta-llm — LLM backends (HTTP SSE + embedded llama.cpp) — **4/5**

**Metrics.** ~1,408 LoC production (lib 133, types 148, error 93, tokens 13, http 463, embedded 558); ~711 LoC tests. 48 pub items. 20 tests default / 32 with `embedded` (3 GGUF e2e additionally `#[ignore]`d). Features: `embedded`, `embedded-cuda`.

**Strengths.** Retry scope deliberately restricted to connect errors + 5xx with an explicit rationale (http.rs:244-249); backoff math exact (250 ms → 500 ms, capped 2 s); byte-level SSE handling with split-UTF-8 safety, exercised by a mock-server `Fragments` step that forces real TCP chunk splits; A8 tool_calls bound with a clear attack comment and correct first-writer-wins assembly; `DeltaEmitter` implements OpenAI `stop` semantics precisely (holdback, multibyte-safe, 5 unit tests); `global_backend()` OnceLock handles llama.cpp's once-per-process init, caching failures as permanent; sampler chain verified against the pinned llama-cpp-2 0.1.156 / vendored llama.cpp sources; feature gating complete — every `Embedded*` reference behind `#[cfg(feature)]`, no ungated references anywhere in the workspace (verified).

**Findings.** F-03, F-05, F-06, F-19, F-20, F-21, F-22; risks R-01, R-16–R-20. No critical/high.

**ADR compliance.** §6.2 COMPLIANT (all six Backend methods, SSE/incremental deltas/finish reason/tool_calls/3 attempts/backoff scope/404 hint/`stream:false`/index bound all verified in code; context default 32768). §6.1 COMPLIANT (ToolCall emitted after last fragment, before Finish). §6.11 DIVERGENT in part (remedies missing on Malformed/StreamFailed/Http). §9 COMPLIANT except the §16.4 blanket claim (F-06). §10 COMPLIANT.

### 4.2 rusta-edit — forgiving SEARCH/REPLACE edit pipeline — **4/5**

**Metrics.** ~1,566 LoC production (parser ~373, apply ~1,049, ledger ~114, lib 30); ~1,695 LoC tests; **70 tests**, all pass. Zero runtime dependencies; dev-only `insta`, `tempfile`.

**Strengths.** A1 fix real and pinned at all three commit paths; **Aider parity verified line-by-line** against `editblock_coder.py` — `perfect_replace`/`replace_part_with_missing_leading_whitespace`, the #25 blank-line retry, the `len(part_pieces)==1` dotdotdots guard, and verbatim failure-feedback f-strings pinned by 5 insta snapshots; single mutation funnel (`guarded`) with a structural scanner test that asserts proof of life (12 primitives, `EXPECTED_INSIDE_GUARDED = 3`); defence-in-depth confinement (lexical + filesystem with deepest-existing-ancestor canonicalization) including the undo path the second audit missed; char-safe whitespace-flexible matching; journal-before-write with rollback; no panics/unwrap/expect/TODO in production code (grep-verified).

**Findings.** F-23, F-24, F-25, F-26; risks R-21–R-25. No critical/high/medium.

**ADR compliance.** §6.3 parser rules 1–6 COMPLIANT; apply chain COMPLIANT (DECIDED no-edit-distance clause honoured); confinement/journaling/ledger COMPLIANT (auto-inject + notify + retry; write-of-unread-existing refused per the one-exception rule); §6.4 DIVERGENT only on CRLF asymmetry (F-24); §6.12 COMPLIANT with symlink hardening beyond the ADR; §9 DIVERGENT on the corpus apply clause (F-23); §16 A1 VERIFIED FIXED.

### 4.3 rusta-core — state machine, context manager, prompt compiler, sessions — **4/5**

**Metrics.** 7 production files, ~2,590 LoC above `#[cfg(test)]` markers (3,773 including in-file tests); 611 LoC integration tests. ~30 pub fns, 12 pub types. **60 tests**, all pass. No features of its own; correctly backend-agnostic.

**Strengths.** `pure_transition` is a compile-enforced exhaustive match, and the pinned-table test additionally asserts its length equals `STATES × PHASE_EVENTS` so a cell can't silently go missing (state.rs:658); `unlocking_path` derives corrective notes by BFS over the transition table itself — notes cannot go stale; `Tool::one_liner` single-sources the §6.4 registry and the prompt compiler; `apply_summary` is one code path for live compression **and** Summary-event replay, pinned byte-for-byte (`replay_reproduces_live_compression_byte_for_byte`); the clip loop has a strict-progress guard with a dedicated termination regression test; torn-tail repair works on open for log and sidecar with mid-file corruption still fatal, and the sidecar is consumed in lockstep before the confinement filter drops escaping edits; canonical-JSON fingerprints provably key-order independent; FNV-1a pinned to published spec vectors. Every prior-round finding touching this crate (C3, C4a/c, G1, G2, A2, A6, A7, L9, L10) re-verified as actually fixed.

**Findings.** F-16, F-17, F-18; risks R-09–R-15. No correctness bug reachable from a production path.

**ADR compliance.** §6.4 COMPLIANT (28 cells pinned with presence + outcome assertions; `LoopEscalated` a first-class edge; replay validates every journaled StateChange). §6.6 COMPLIANT with F-16 caveat. §6.10 COMPLIANT (all 15 event variants, FNV-1a sidecar consumed in lockstep, torn-tail both directions, 0600 on create and re-tighten, tombstones, rebuild confinement equals replay confinement). §6.11 COMPLIANT (minor R-12). §6.1 COMPLIANT (A25 fix verified).

### 4.4 rusta-repomap — tree-sitter repo map — **4/5**

**Metrics.** 8 files, **1,131 LoC** production; 484 LoC tests; 269 lines of `.scm` queries (data). **19 tests**, all pass. ~8-item public API.

**Strengths.** All seven `.scm` queries **byte-identical** to Aider's reference tree, with `lang.rs` compile-testing all 8 `Lang` variants against the pinned grammars (the §13 grammar-drift guard is real and exercised); `graph.rs` reproduces `repomap.py` faithfully — damping `mul·√n_r`, the full boost ladder (×10 mention, ×10 shape counted in chars not bytes, ×0.1 underscore, ×0.1 |D|>5, ×50 chat-referencer), 0.1 self-edges, networkx-equivalent dangling-mass redistribution, checked line-by-line against networkx's `pagerank_alg`; `estimate_tokens` a faithful port of Aider's sampling trick; C1 regression pinned twice including at the §7 default 1024 budget on a non-trivial repo; stale-line safety explicit and tested; cache invalidation tested with a real mtime bump; A5 dead-mentions fix verified wired end-to-end (agent.rs:358 → `set_mentions` → `render_map`).

**Findings.** F-07, F-08, F-09, F-10; risks R-26–R-30. No critical/high.

**ADR compliance.** §6.5 steps 1–8 all COMPLIANT (discovery, tags+backfill, graph ladder, PageRank, rendering, token fitting, cache, drill). The step-8 DECIDED clause's "constant shared with the renderer" phrase is stale relative to its own 2026-09-13 revision (code follows the revision — an ADR-internal wording inconsistency, not a code defect). §10 and R10 COMPLIANT.

### 4.5 rusta-tools — ten-tool phase-gated registry — **3.5/5**

**Metrics.** 10 files, ~1,739 LoC production; ~1,433 LoC tests. ~47 pub items. **48 tests**, all pass. No features (correctly — `embedded` gating lives elsewhere and is consumed opaquely).

**Strengths.** `read` streams via `read_until` with exact `str::lines` semantics and cap-check-before-push, with the round-6 window regression pinned; shell pipe draining bounds *memory* (each stream through `capped_read`, concurrent join), pinned by an RSS-measuring regression test; timeout-kill verified behaviourally with actionable remedy; `edit`/`write` funnel into rusta-edit's single apply chain; `write` refuses blind overwrites of unread files, tested both directions; **the state×tool matrix executes the production `exec` path for every cell, asserts blocked cells leave bytes untouched and spawn no process, and proves the allowed direction actually mutates — not vacuous**; sub-coder isolation real (ReadOnly bypasses `exec`, reads credit nothing, transcripts side-channeled); glob matcher memoized with pathological-pattern timing tests.

**Findings.** **F-01 (high)**, F-02 (medium), F-11, F-12; risks R-02, R-03, R-04, R-31–R-34.

**ADR compliance.** §6.1 COMPLIANT with caveats (every tool caps its observation; read/shell bound memory; grep/glob don't bound walk memory; map_drill loads whole files; marker-charging sentence ambiguous — see F-13/R-31). §6.4 COMPLIANT (single table sources `available_in`/`tools`/`corrective_note`; gate order as specified; unreachable-in-read-only-states by construction, pinned both directions). §6.12 **DIVERGENT in two places**: the read-side confinement omits `grep` (F-01 — the ADR text explicitly names it) and the deny table covers only a subset of its stated catch-all (F-02); approval injection, minimal env, timeout kill, cwd=repo-root, stdin closed, interactive denial all COMPLIANT and tested. §6.8 COMPLIANT. §6.11 COMPLIANT.

### 4.6 rusta-dispatch + rusta-validate — sub-coder dispatch + Reflexion gate — **4/5**

**Metrics.** dispatch: 807 LoC production (actor 272, dag 319, toolcall 191, lib 25); 20 tests. validate: 1,086 LoC production (validators 1,058 incl. ~345 test lines); 17 tests. All pass; `cargo check -p rusta-cli --features embedded` clean.

**Strengths.** `cap_report` charges its clip marker against the 400-token cap, pinned by an exact-token assertion, with a comment recording the ~411-token bug this prevents; sub-coder read-only enforcement is **two independent layers** (the `SUB_CODER_TOOLS` filter *and* the host ReadOnly executor matching only five handlers); ledger isolation real (ReadOnly discards `read_credit`, pinned by test); `trim_partial_utf8` is careful byte-walking boundary repair whose test was rewritten to feed exactly the shape `capped_read` produces — §16.2 applied to itself; feedback budget arithmetic reserves one line per header so the 30-line cap holds by construction; feature gating hazard-aware (rusta-cli forwards `embedded` to both `rusta-llm` and `rusta-dispatch` with a comment naming the non-exhaustive-match failure it prevents).

**Findings.** F-13, F-14, F-15; risks R-05, R-35–R-39. No critical/high/medium verified.

**ADR compliance.** §6.8 COMPLIANT (task/tasks, max 4, distinct labels, 5 read-only tools, fresh contexts, 6-turn cap + wrap-up, ≤400-token reports, labeled reports in input order, RESEARCH FAILED on backend failure, panicked-actor-keeps-label untested (R-37), no ledger credit, transcripts to session log only, embedded serialization verified — e2e asserts `max_inflight == 1` in Serialized mode). §6.1 DIVERGENT at the observation level (F-13). §6.7 COMPLIANT (every element present and pinned, including the live phase-machine matrix `gate_wires_the_verifying_exit_gate`; `Verdict::event` consumed correctly at agent.rs:810-829). §6.10/§6.11/§6.2 COMPLIANT.

### 4.7 rusta-cli — CLI host — **4/5**

**Metrics.** 8 files, ~3,095 LoC production (agent 1,151, commands 517, config 457, repl 458, git 352, lib 110, render 37, main 13); ~1,097 LoC tests. ~63 pub items. **38 tests**, all pass.

**Strengths.** `parse_items` interleaves tool calls and edit blocks in true document order using `rusta_edit::BlockScan` as the seam arbiter; the F1 regression (a ```` ```tool ```` fence *inside* a SEARCH/REPLACE body is content) pinned at unit and corpus level; `/undo` commit selection structurally sound — `reset_if_head` compares HEAD to the recorded sha before any reset, and the round-trip test interleaves a user's own commit between two rusta commits and proves the unrelated HEAD is never reset; A3 remediations real (BatchBoundary/UndoApplied journaling, `rebuild_batches` applying the same confine filter replay does, two e2e regressions walking `/undo` past a resume boundary); transport failure vs Ctrl-C correctly distinguished with a never-stranded-phase regression test; `read_user_line` guards `block_in_place` by runtime flavor; config has `deny_unknown_fields`, rejects `max_turns == 0`, and test-pins the example config against schema drift; journaling failures reported once per session with remedy.

**Findings.** F-04 (medium), F-27, F-28, F-29; risks R-06, R-07, R-40–R-45.

**ADR compliance.** §6.1 COMPLIANT (document-order execution, prose ends turn, Ctrl-C aborts/discards/preserves; R1 wrap-up nuance pinned by test). §6.4 COMPLIANT (conservative `is_plan` tested both directions, AutoGate, `/auto` shares one flag with shell approval, one-ask bound with e2e test). §6.7 COMPLIANT (fresh gate budget per request, exhaustion surfaces with `/undo` remedy). §6.9 COMPLIANT (`rusta: <summary>` tested, reset-only-if-HEAD, graceful no-git degradation, all 14 commands present, `-c` auto-approves plans and denies shell — verified). §7 COMPLIANT (discovery nearest-first tested, precedence test). §6.10 COMPLIANT (full-state replay; A3 regressions strong).

### 4.8 Cross-Crate Wiring & Workspace Infrastructure — **4/5**

**Metrics.** 49 production `.rs` files, 12,337 LoC; 8,603 test LoC; ~345 pub items across the workspace. Graph: llm/edit/repomap (leaves) → core → dispatch/validate → tools → cli.

**Strengths.** Dependency inheritance total and uniform — every external dependency in all 8 member manifests uses `dep.workspace = true`; zero version literals outside the root; `Cargo.lock` has no duplicate name+version pairs; **`embedded` forwarding verified by execution** (`cargo tree --features embedded -i llama-cpp-2` shows the full chain); layering is a strict DAG matching §5 with no cycles and no upward deps, and cli's direct deps on dispatch/validate are genuinely used; error-seam discipline per §6.11 (anyhow only in rusta-cli, 3 sites; everywhere else thiserror; the only production `expect`s at seams are provably safe); CI runs every gate §12/§17 claims — fmt, clippy on both feature graphs (separate embedded job), tests default + embedded, LoC gate, cargo-deny with `all-features = true`, plus the §9-mandated feature-forwarding check; the LoC script is robust (handles `#[cfg(all(test,…))]`, xargs splitting) and reproduces §17 exactly; example-config drift is test-pinned; **every quantitative claim in §17 reproduced precisely** (292 tests, 12,337/8,603/20,940 LoC).

**Findings.** F-30, F-31, F-32, F-33, F-34, F-35; risks R-08, R-46, R-47. No critical/high/medium.

**ADR compliance.** §5 COMPLIANT (doc-level path/features comments diverge, F-30/F-33); §7 DIVERGENT (F-31 undocumented key); §9 COMPLIANT in substance; §10 COMPLIANT (reedline default-features off, exact pins, encoding_rs isolated behind embedded, nothing outside the allowed set); §12 COMPLIANT with F-32 boundary caveat; §13 COMPLIANT; §16/§17 claims re-verified COMPLIANT.

---

## 5. Reference-Project Fidelity (semantic parity checks)

Performed against the latest trees in `/home/vladimir/develop/refs/`:

| Rusta component | Reference | Parity result |
| --- | --- | --- |
| Edit parser + apply chain | Aider `aider/coders/editblock_coder.py` (657 L, read in full) | **Faithful port.** Marker rules, whitespace-flexible matching, blank-line retry, `...` elision with the single-piece guard, cross-file retry, and failure-feedback strings match the reference; all 25 prior findings touching this seam re-verified fixed. Documented, low-severity divergences: first-vs-best fuzzy file match (F-26), unported filename-line strip (R-24), line-based vs regex dot-line split (R-23), best-window similarity metric (R-25). |
| Repo-map queries | Aider query packs + `tree-sitter-languages` | **Byte-identical** across all seven languages. |
| Repo-map ranking/rendering | Aider `repomap.py` + networkx `pagerank_alg` | **Faithful.** Full boost ladder, `mul·√n_r` damping, dangling-mass via personalization vector, token-estimate sampling trick, `≤ budget` fitting (stricter than Aider's 15% tolerance, per ADR). Recorded ADR-compliant divergences: `+=` vs `max` for mentioned-file, N = tagged files, no `references=defines` fallback (R-30), tie-break order (R-28). |
| little-coder (skill cards) | `little-coder` front-matter schema | Starter deck compiles into the binary with ≤120-token bodies, ≤2 cards/request; parse enforced at load **and** CI-tested. No divergence found. |
| SmallCTL (phase machine) | §6.4 registry/gate | Single-sourced tool availability between registry and prompt compiler; blocked calls physically unreachable. No divergence found. |
| Observer / smallcode | Dispatch + edit recovery patterns | Sub-coder isolation two-layered; recovery parser survives missing fences and stray markdown per the smallcode/Aider philosophy. No divergence found. |

---

## 6. Themes (cross-cutting analysis)

1. **The A10 class recurs where fences were applied selectively.** F-01 (grep) and F-07 (repomap render) are both "one path got the fence, a sibling didn't" instances of the same accepted finding class — and §16.2's acceptance rule ("check every consumer") exists precisely for this. **Recommendation: one shared confinement primitive used at every read/mutation site, with a structural test (like rusta-edit's mutation-funnel scanner) that fails the build when a new `fs::read`/`fs::write` appears outside it.**
2. **"The guard says so" residue at untested paths.** F-05, F-06, F-13+F-14, F-23, R-37: every guard on a path no test exercises was either missing (A8 test), inconsistent with its sibling (clip_observation), or silently bounded nothing (corpus apply expectations). This is the project's own measured failure mode, concentrated where coverage honestly ends.
3. **Cap-marker charging is inconsistent across §6.1 consumers.** Honored in `cap_report`; violated in `clip_observation` (F-13); ambiguous at grep-line/shell caps (R-31). One rule, three behaviours — needs an ADR ruling and a shared helper.
4. **Model-facing outputs that are silently wrong.** F-03 (truncation as Stop), F-11 (fabricated read range), F-09 (`path:1-0`), F-27/F-29 (false user-facing messages). Each is small; together they erode the exactness the harness sells. All are cheaply fixable and testable.
5. **Documentation drift in the normative documents.** F-17, F-18, F-19, F-22, F-30–F-33, R-35 — the ADR and doc comments have begun to diverge from the code in the same way earlier rounds policed in the opposite direction. §0's numbering contract is intact; the *content* contract needs a drift pass.
6. **Async-runtime hygiene at the I/O edges.** R-02, R-03, R-04: blocking sync I/O and unbounded buffers at walk/drill/render paths; no `spawn_blocking`, no walk caps. Latent today (target repos are small), real at scale.

---

## 7. Recommendations (prioritized)

| # | Priority | Action | Addresses |
| --- | --- | --- | --- |
| 1 | **P0** | Add confinement to `grep` (search.rs:103 — `contains_path`/`safe_rel_in` like its siblings) and to repomap's render read closure (lib.rs:127); extend the A10 regression to cover all three named tools **and** the render path; add a structural no-unfenced-IO test. | F-01, F-07 |
| 2 | **P0** | Track `[DONE]` in the SSE pump; on stream end without it, emit `StreamEvent::Failed` with a remedy (truncated-output semantics), and add a mock-server regression for mid-stream disconnect, mid-stream malformed JSON, and empty `data:` keep-alives. | F-03, F-05, F-20 |
| 3 | **P1** | Make `/undo` failure-atomic: restore journal entries before popping (or push back on failure), and skip `reset_if_head` unless all entries restored; distinguish "reset failed" from "HEAD moved on" in the message. | F-04, F-27 |
| 4 | **P1** | Close the shell deny-table gap: add `~`/`$HOME`/traversal-aware write patterns (or make the 9th rule a post-resolution path check, not a regex), and test each bypass string. | F-02 |
| 5 | **P1** | Add the missing A8 regression test (out-of-range tool_calls index ≥ 64) and honest coverage for `clip_observation`, the corpus apply expectations (§9), and the panicked-actor fallback. | F-06, F-13, F-14, F-23, R-37 |
| 6 | **P2** | One shared cap-marker helper charged against every cap (grep line, shell bytes, clip_observation); rule the §6.1 sentence to cover all caps; clamp `read`'s `last_line` to `total_lines` and drill's empty-window header; fix `capsule_note`'s first-capsule bypass. | F-11, F-13, F-16, F-09, R-31 |
| 7 | **P2** | Documentation drift pass: state.rs header/test docs (6→7 events, 24→28 cells), ADR §9 corpus path, document `timeout_secs` in §7 + example, root-features comment, remove stale `wait_with_output` comment, correct the TSX parity claim, fix the §6.11 remedy doc claim. | F-17–F-19, F-30–F-33, F-10, F-12, F-22 |
| 8 | **P3** | Harden discovery edges: `git -c core.quotepath=false ls-files -z` (F-08); word-boundary the ```` ```tool ```` fence match (F-28); honor `--backend` in the journaled config line (F-29); empty-brief validation symmetry in dag.rs (F-15); CRLF normalization symmetry between edit syntaxes (F-24). | F-08, F-28, F-29, F-15, F-24 |
| 9 | **P3** | CI: add the `cargo doc` step that §17 claims as a gate (F-34); consider `deny` multiple-versions deny (F-35); consider embedded-graph macOS CI (R-08). | F-34, F-35, R-08 |

---

## 8. Final Assessment

Rusta is **substantially what its ADR claims**: a milestone-complete, heavily self-audited harness whose quantitative gates all reproduce, whose reference-project ports are semantically faithful under line-by-line comparison, and whose prior six rounds of findings are genuinely fixed in code rather than merely asserted. The workspace wiring — dependency inheritance, feature forwarding, DAG layering, error-seam discipline — is the strongest part of the project and verified by execution, not manifest reading.

This seventh round found **no critical issues, one high (F-01), six medium (F-02…F-06 plus the F-07 borderline), and thirty-five low** verified findings. The pattern is more informative than the count: defects concentrate at (a) paths adjacent to previously-fenced paths, (b) guards on untested paths, and (c) normative documents drifting from code — all three being the exact classes this project's own §16 acceptance rules were written to catch, now caught by them in round seven. None touches the architecture; all are cheaply fixable, and recommendations are ordered accordingly.

The two risks no audit can close remain the ones ADR §17 owns honestly: **zero real-model exposure** (every completion processed so far was hand-written by a test), and the fundamental **guard-rail-not-sandbox** nature of §6.12. This report therefore supports the ADR's own claim and no stronger one: *pre-alpha, milestone-complete, not production-ready.*

---

*Method appendix: 8 parallel deep-audit passes (7 crate scopes + cross-crate wiring), each reading every production line of its scope against the full ADR; reference-parity passes against `/home/vladimir/develop/refs/{aider,little-coder,Observer,smallcode,SmallCTL}`; an independently executed gate run (fmt, clippy ×2 feature graphs, tests ×2 graphs, doc, LoC) on 2026-09-17; and lead re-verification by direct source read of every high/medium finding (marked **[lead]**) before inclusion. Findings not reproducible were quarantined to §3 rather than reported.*
