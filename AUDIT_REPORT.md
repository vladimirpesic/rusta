# Rusta — Consolidated Audit Report (Round 8)

**Commit audited:** `63e1fb5` (HEAD → main) · **Date:** 2026-09-17
**Normative baseline:** `ADR.md` v1.0, §0–§17
**Sources consolidated:** `RUSTA_AUDIT_REPORT_GLM.md`, `RUSTA_AUDIT_REPORT_KIMI.md`

**Method.** Every finding below was re-verified against the code at `63e1fb5` before inclusion —
by **execution** where a probe could be written, otherwise by reading the cited `file:line`.
Findings that did not reproduce, or that reproduced differently from the claim, are recorded in
§5 rather than dropped. Findings the source reports raised that this round did **not**
independently verify are listed in §6 as carried-forward, not asserted as confirmed. Temporary
probes were removed; the working tree is clean apart from the two source reports.

**Finding IDs continue the ADR's `A`-series** (which ended at A25 in §16.4), so this report folds
into ADR §16 without renumbering. The `Origin` column maps each to its source report's ID.

**Gate status at `63e1fb5` (re-executed this session):** `cargo fmt --all --check` clean ·
`cargo clippy --workspace --all-targets -- -D warnings` **0 diagnostics** ·
`cargo test --workspace` **292 passed, 0 failed** · `scripts/loc_budget.sh`
production **12,337** / 15,000 · tests 8,603 · total **20,940** / 25,000.
Both source reports' gate claims reproduce exactly.

**Every finding in this report sits behind those green gates.** That is the project's standing
condition, not a new one.

---

## 1. Verdict

**1 High · 6 Medium · 21 Low**, plus **A54**, found during remediation rather than by either audit. No finding is remotely exploitable by a third party; R11 holds
(the only network path is the user-configured backend). The High finding is an information-flow
defect: `grep` reads outside the repository through an in-repo symlink, and it reproduces in two
lines of probe code.

The dominant theme is **the same three shapes ADR §16.1 names**, in their purest form yet:

- **an unfenced path** — A26, A32: two of the three read tools named by §6.12's own revision got
  the fence; `grep` and the map renderer did not;
- **an inert safeguard** — A30, A31: the A8 bound and the entire `StreamEvent::Failed` path ship
  with no test that exercises them;
- **a claim the code does not make** — A39–A51: eleven documentation defects, several of them in
  the load-bearing files whose contract is exactness.

### Reconciling the two source reports

**The two reports are not two independent opinions.** `RUSTA_AUDIT_REPORT_GLM.md` is, to within
six blank lines and one code-fence label, **byte-identical to the round-7 report already in this
repository's git history** (`30164b2 docs: add round-7 standalone QA audit report`, removed by
`63e1fb5`). Diffed and confirmed. It therefore contributes no new information to this round; it
is the previous round's report re-presented.

| | GLM (= round-7 report) | KIMI | This report |
| --- | --- | --- | --- |
| Verdict | "PASS … **No new High or Medium findings**" | 1 High, 5 Medium, 29 Low, 47 quarantined risks | 1 High, 6 Medium, 21 Low |
| Method | Structure/gates re-run; findings by reading | `file:line` + reproduction per finding | Re-verification by execution |
| Outcome | Missed the High entirely | High reproduced; substantially correct | — |

**This is round 5's lesson repeating exactly.** ADR §16.3 records that one external reviewer
verified each guard *exists* and read what each guard *says about itself*, while the other
reproduced its claims — and the second was right. The same split recurs here. The GLM/round-7
report states that "every specified cap and fence that this audit could exercise by execution
held" and that "no defects of the §16.1 recurring shapes … were found". Both statements are
false at `63e1fb5`: A26 is a §16.1 unfenced path, and a five-line probe exposes it.

Worth stating plainly, because it cuts against this project's own documentation: **ADR §16.4's
claim that all 25 prior findings are "pinned by a regression test" is not true** (A31), and
**ADR §16.2's rule that a fix must be checked against every consumer was itself applied to only
two of the three tools §6.12 names** (A26). The ADR is a normative document that has drifted
from the code it governs — which is precisely the defect class §16 exists to police.

---

## 2. High

### A26 — `grep` reads outside the repository through an in-repo symlink

*Origin: KIMI F-01 · `crates/rusta-tools/src/search.rs:90-108` · **recurrence of A10***

§6.12's read-side revision names `read`, `grep` **and** `map_drill`. `read` resolves through
`safe_rel_in`; `map_drill` resolves through `safe_rel_in`; **`grep` calls `fs::read(root.join(&rel))`
with no confinement check at all.** `walk_dir` pushes symlinked files (a symlink to a file is not
a directory, so it falls through to the `strip_prefix` branch), and `fs::read` follows them.

Reproduced by execution in a scratch repo containing `link.txt → <outside>/secrets.txt`:

```text
F-01 grep  status=Ok    leaked=true   content="link.txt:1: API_KEY=hunter2"
F-01 read  status=Error leaked=false
```

`grep` returns the outside-repo content as a successful observation; `read` refuses the same
path. The content enters the model context *and* the session log. It is also reachable from
sub-coder `grep`, so it crosses the §6.8 isolation boundary that dispatch advertises.

The A10 regression test is named `read_tools_refuse_a_symlink_out_of_the_repo` — plural — and
**exercises only `read`** (`crates/rusta-tools/tests/regressions.rs:473-505`; confirmed by
inspection: no `grep` or `map_drill` call appears in it). This is the §16.2 acceptance rule
failing on its own terms.

**Fix:** route `grep`'s per-file read through `safe_rel_in` (or `contains_path`), and extend the
A10 test to all three tools the erratum names.

**Status: RESOLVED** (wave 1). Fenced via `rusta_edit::contains_path`; pinned by
`every_read_side_tool_refuses_a_symlink_out_of_the_repo`, which was confirmed red against each
fix independently before being accepted.

---

## 3. Medium

### A27 — The shell deny table does not implement its own documented 9th rule

*Origin: KIMI F-02 · `crates/rusta-tools/src/shell.rs:64-129`*

§6.12 lists "any write outside the repo root" among the default denials. Executing
`ShellPolicy::check` on each candidate:

```text
rm -rf /                       -> Denied  { "rm targets the filesystem root" }
sudo apt install x             -> Denied  { "sudo escalates privileges" }
echo x > /etc/passwd           -> Denied  { "writes outside the repo root" }
rm -rf ~                       -> Allowed
rm -rf $HOME                   -> Allowed
echo x > ~/f                   -> Allowed
tee ~/out                      -> Allowed
mv f /tmp/../etc/cron.d/x      -> Allowed
cp f ../outside                -> Allowed          ← beyond the source report's list
```

Absolute paths into listed system directories are caught. `~`/`$HOME` spellings, traversal
spellings that do not begin with a listed directory, and `cp`/`mv` to a relative `../` target are
not. The approval gate still stands in front of all of these — but `/auto` skips it, and
`[shell] allow` prefixes bypass it, which is exactly when the static table is the only defence.

**Fix:** either extend the table to the spellings above, or amend §6.12 to describe the
table's real scope. The current text promises more than the code delivers.

**Status: RESOLVED** (wave 2) — **both**, because neither alone is honest. The table now tests
the three spellings of "outside" (absolute, `~`/`$HOME`, `../` traversal) instead of enumerating
system directories, and tests the *final* argument for `cp`/`mv` so reading from outside into the
repo stays allowed. §6.12 now states what a regex table over a Turing-complete shell can and
cannot guarantee: a filter against a confused model, not an adversarial boundary. Pinned by a
33-command corpus asserting **both** directions — 21 must-deny and 12 must-allow, because
widening a deny list breaks legitimate commands and a shell tool that cries wolf gets `/auto`-ed
past.

### A28 — Silent truncation when a stream ends without `finish_reason` or `[DONE]`

*Origin: KIMI F-03 · `crates/rusta-llm/src/http.rs:411-431`*

`pump_events` ends when the byte stream ends and emits
`StreamEvent::Finish(finish.unwrap_or(FinishReason::Stop))`. `[DONE]` is recognised at line 363
but never *recorded*, so nothing distinguishes "the server said it was finished" from "the
connection dropped". A mid-stream disconnect is reported as a clean `Stop`; the agent breaks on
`Finish` and commits the partial text as the turn's answer.

For a harness whose output is anchored on by SEARCH blocks, truncated-but-reported-complete is
the same failure class as the round-6 `read` regression: silently wrong content beats a loud
error only if you never find out.

**Fix:** track whether `[DONE]` or a `finish_reason` was seen; emit `Failed` (or a distinct
`FinishReason`) otherwise.

**Status: RESOLVED** (wave 2). Deliberately narrow: a server that omits `[DONE]` but sends a
`finish_reason` is still believed, so only the genuinely ambiguous case fails. Both directions
pinned. With the check disabled the test reports the defect verbatim — `[Delta("partial ans"),
Finish(Stop)]`.

### A29 — A partial `/undo` failure destroys the rest of the batch and still reverts the commit

*Origin: KIMI F-04 · `crates/rusta-cli/src/commands.rs:186-222` + `crates/rusta-edit/src/apply.rs:125-140`*

Three facts compose into an unrecoverable state:

1. `commands.rs:187` pops the batch off the stack **before** restoring anything.
2. `UndoStack::undo_last` pops the journal entry, *then* calls `guarded(...)?` — the `?` returns
   after the pop, so a failed write destroys the entry.
3. On `Err` the loop `break`s, and execution proceeds **unconditionally** to the
   `match &batch.sha` arm that calls `reset_if_head`.

Result: some files restored, the rest of the journal gone, the commit reverted anyway, and the
batch no longer on the stack — so the next `/undo` reports "nothing to undo". Working tree and
git history disagree with no path back.

Partially mitigated: `UndoApplied { entries: restored.len() }` journals the true count, so replay
stays self-consistent. The on-disk state does not.

**Fix:** restore before popping (or re-push on failure), and make the commit revert conditional
on a complete restore.

**Status: RESOLVED** (wave 2), all three composing mistakes: `undo_last` pops the journal entry
only after the restore succeeds; the batch stays on the stack reduced to what is still
unrestored; and the commit revert is skipped entirely on a partial restore. A retry after fixing
the disk now finishes the undo. Pinned by a test that injects a real write failure (`0o444`) and
fails loudly rather than skipping if the injection does not take.

### A30 — No test exercises `StreamEvent::Failed` or a truncated stream

*Origin: KIMI F-05 · `crates/rusta-llm/tests/`*

Across `mock_server.rs`, `http_e2e.rs` and `embedded_e2e.rs`, the only occurrence of `Failed` is
a `panic!` arm in `embedded_e2e.rs:72` — an assertion that it does *not* happen. No test produces
a mid-stream failure, a malformed chunk, or a stream that ends early. A28's behaviour and A31's
bound both ship on paths nothing has ever run.

**Status: RESOLVED** (wave 2). Three `Step::Fragments` tests now drive the failure path: a cut
stream, a `finish_reason` without `[DONE]`, and an out-of-range `tool_calls` index.

### A31 — The A8 fix has no regression test, and ADR §16.4 claims otherwise

*Origin: KIMI F-06 · `crates/rusta-llm/src/http.rs:382-392` vs `ADR.md` §16.4*

The guard is present and correct: `MAX_TOOL_CALLS = 64`, `index >= MAX_TOOL_CALLS` →
`Error::Malformed`, with a comment recording the ~309 GB allocation it prevents. **No test
anywhere in the workspace exercises it** — a grep for out-of-range index patterns finds only the
implementation and its own comment.

ADR §16.4 states: *"All 25 findings were resolved and each is pinned by a regression test."*
That sentence is false, and it is load-bearing — it is the sentence a future reader would rely on
to skip re-verification. This is the §16.6 rule 3 failure applied to the project's own record.

**Status: RESOLVED** (wave 2). The bound is now pinned by
`an_out_of_range_tool_call_index_is_rejected_not_allocated`, and ADR §16.4's claim is corrected
to say that "pinned by a test" is a claim to check rather than a conclusion. Proving this guard
by reverting it was declined deliberately: doing so would attempt the 309 GB allocation it
exists to prevent, so the test was instead shown to discriminate on the index value (63 passes
clean, 4294967295 fails).

### A32 — The map renderer follows symlinks out of the repository

*Origin: KIMI F-07 (graded Low there; raised here) · `crates/rusta-repomap/src/lib.rs:127`*

`render_map`'s read closure is a plain `fs::read_to_string(root.join(rel))` with no containment
check. Reproduced with a git-tracked `src/linked.rs` pointing outside the repo:

```text
F-07 map_refresh  status=Ok    leaked=true
     content="\nsrc/linked.rs:\n│pub fn leaked_secret_fn() {}\n\nsrc/ok.rs:\n│pub fn normal() {}\n"
F-07 map_drill    status=Error leaked=false
```

Outside-repo source is rendered into the map under a repo-relative header, while `map_drill` on
the identical path refuses. **Graded Medium rather than Low** because it is the same
information-flow defect as A26, reproduces identically, and `map_refresh` runs automatically —
it does not require the model to ask for the file by name.

**Status: RESOLVED** (wave 1). Fenced *before* tag extraction rather than at the render read,
because tags carry identifier names out even when source lines are withheld; both `chat` and
discovered files funnel through that one loop.

---

## 4. Low

Verified at the cited location; reproduced by execution where marked **[exec]**.

| # | Origin | Finding | Location |
| --- | --- | --- | --- |
| **A33** | F-08 | **Status: RESOLVED** (wave 3) — `git ls-files -z`. **[exec]** Non-ASCII filenames silently vanish from the map in git repos. `git ls-files` C-quotes them (`"src/caf\303\251.rs"`), rusta takes the output literally, the path never matches, and the file disappears with no warning. Probe: `src/café.rs` absent from the map, `src/plain.rs` present. The non-git walk fallback handles the same name fine, so git and non-git repos diverge. | `rusta-repomap/src/discover.rs:37-40` |
| **A34** | F-09 | **Status: RESOLVED** (wave 3); see A54, which this fix uncovered. **[exec]** `drill` on an empty file returns the contradictory range `empty.rs:1-0` (`from` clamps to `max(1)`, `to` clamps to `0`). See §5.1 — the model-facing string differs. | `rusta-repomap/src/drill.rs:75-77` |
| **A35** | F-11 | **Status: RESOLVED** (wave 3) — a window entirely past EOF is now an error naming the real length; partial overlap still succeeds. **[exec]** A `read` window starting past EOF fabricates a range. Probe: 3-line file, `read(from:10,to:12)` → `status=Ok`, `truncated=false`, content `"a.rs:3-9\n"` — a header claiming lines 3–9 of a 3-line file, empty body, no remedy. | `rusta-tools/src/read.rs:47,104-111` |
| **A36** | F-13/F-14 | **Status: RESOLVED** (wave 3). `clip_observation` appends its marker *past* the cap, while the sibling `cap_report` subtracts it first — and carries a comment recording the ~411-token bug that taught the lesson. §6.1 says a cap's marker is charged against the cap. The function also has zero test coverage. | `rusta-dispatch/src/actor.rs:168-177` |
| **A37** | F-15 | Input-validation asymmetry: `{"task": ""}` bypasses the non-empty check the `tasks` array form enforces (`from_items` filters `!t.trim().is_empty()`; the single-task arm does not), spawning a sub-coder with an empty brief. | `rusta-dispatch/src/dag.rs:77` vs `99-103` |
| **A38** | F-16 | `capsule_note`'s budget guard is `> CAPSULE_TOKEN_BUDGET && included > 0`, so a *first* capsule over 180 tokens ships regardless — the §6.6 guarantee has a hole. Latent: the shipped set's longest text is ~25 tokens. | `rusta-core/src/context.rs:1122-1126` |
| **A39** | F-17 | Module doc says "Four states, **six** scaffold events" — `PHASE_EVENTS: [PhaseEvent; 7]` since `LoopEscalated`. | `rusta-core/src/state.rs:3` |
| **A40** | F-18 | The M3 acceptance test's own doc says "all **24** `(State × PhaseEvent)` cells … all **sixteen** illegal combinations"; the table is 4 × 7 = **28**. Assertions are correct; only the prose miscounts. With A39, both load-bearing spots in `state.rs` miscount the table they exist to pin. | `rusta-core/src/state.rs:525-527` |
| **A41** | F-23 | ADR §9 says each corpus fixture carries "an expected parse **and** an expected apply result". `parser_corpus_is_green` is the only fixture-driven test and calls **only** `parse_response`; no fixture is ever run through `Editor`. The `Editor` uses in `corpus.rs` are hand-written cases. | `rusta-edit/tests/corpus.rs:155-195` vs `ADR.md:735-738` |
| **A42** | F-24 | **Status: RESOLVED** (wave 3). CRLF forgiveness is asymmetric across the two syntaxes §6.4 requires to be identical: text blocks are normalized, but `prep()` does not normalize `\r` and the tool-call `edit` path passes `search`/`replace` verbatim — a model emitting `\r\n` in a tool-call `search` can never match. | `rusta-edit/src/apply.rs:631-638` + `rusta-tools/src/edit.rs:22-26` |
| **A43** | F-27 | **Status: RESOLVED** (wave 3) — `reset_if_head` returns an outcome enum, so a failed revert is reported as one. `reset_if_head` returns `false` both when HEAD moved **and** when `git reset` itself fails; the caller prints "commit kept — HEAD moved on after it" in both cases. On a failed reset that explanation is simply false. | `rusta-cli/src/git.rs:131-140` + `commands.rs:218-222` |
| **A44** | F-28 | **Status: RESOLVED** (wave 3). The tool fence is matched by prefix — `trimmed.starts_with("```tool")` also matches ` ```toolbox ` / ` ```tools `. Such a fence is consumed as a tool call, its body becomes a spurious malformed-block note, and any SEARCH/REPLACE inside it never reaches the edit parser. | `rusta-cli/src/agent.rs:244` |
| **A45** | F-29 | `Config::summary()` journals `self.backend.kind` — the **file's** value — ignoring the `--backend` override that the adjacent accessor honours. One `SessionStart` event can record `backend=http` beside an embedded backend. | `rusta-cli/src/config.rs:240-250` |
| **A46** | F-30 | ADR §9 locates the parser corpus at `crates/rusta-edit/tests/edit_corpus/`; it lives at workspace-root `tests/edit_corpus/`. Loaders point at the real path; the normative document does not. | `ADR.md:735` |
| **A47** | F-31 | `[validate] timeout_secs` is parsed and validated but appears in neither ADR §7's schema nor `rusta.toml.example` (whose `timeout_secs` is under `[shell]`). The inverse of the `strict_grammar` archetype: real, working, undocumented. | `rusta-validate/src/validators.rs:84-89` |
| **A48** | F-32 | The §12 gate scans only `crates/`; any `.rs` under workspace-root `tests/`, `benches/` or `examples/` escapes R1 counting. Harmless today (root `tests/` is `.md`-only), but §12's contract says "counted on `*.rs`" with no such boundary. | `scripts/loc_budget.sh:30,41` |
| **A49** | F-33 | ADR §5's layout comment describes the root manifest as "features: embedded, embedded-cuda (opt-in)"; the root `Cargo.toml` defines **no** `[features]` section — they live on the member crates. | `ADR.md:140` |
| **A50** | F-34 | **CI has no `cargo doc` step** (it runs fmt, clippy, test, loc_budget on the default graph and test+clippy on the embedded graph), yet ADR §17 reports "`cargo doc` 0 warnings" as a gate and ~650 doc comments cite ADR sections. Nothing prevents doc-warning drift. | `.github/workflows/ci.yml` |
| **A51** | F-35 | `multiple-versions = "warn"` — a dependency duplication warns but never fails CI. Moot today. | `deny.toml:26` |
| **A52** | F-10 | The parity justification "TSX shares TypeScript's tags query (**Aider does the same**)" is unsupported: Aider's query packs contain `typescript-tags.scm` and no TSX query, and `repomap.py` does not mention `tsx` at all. Rusta's choice is a reasonable superset and breaks no ADR rule; the cited justification is false. | `rusta-repomap/src/lang.rs:70` |
| **A53** | F-12 | Stale comment: "the timeout's drop of the `wait_with_output` future guarantees the child is killed" — the code builds a custom `collect` future; `wait_with_output` appears only in comments. | `rusta-tools/src/shell.rs:302` |

### A54 — `map_drill` answered a past-EOF window with content from a *different* region

*Found during wave 3 while fixing A34; not reported by either source audit ·
`crates/rusta-repomap/src/drill.rs`*

A34 reported the empty-file header `path:1-0`. Fixing it surfaced the same clamp doing something
worse on a non-empty file. `drill(a.rs, from: 10, to: 12)` on a three-line file returned:

```text
Ok("a.rs:3-3\nthree\n")
```

Real content, from a region the caller never asked for, with a header that matches what was
returned rather than what was requested — and `map_drill` credits the read-before-edit ledger for
it, so the model proceeds believing it has seen lines 10–12.

This is the round-6 `read` regression exactly: *silently wrong content is worse than the error it
replaced*, because a SEARCH block gets anchored on it. It is recorded separately from A34 because
the severity differs — an obviously broken `1-0` header is noticed; a plausible `3-3` header is
not.

**Status: RESOLVED** (wave 3), with A34, by the same `PastEof` error. Partial overlap remains a
success: `from: 2, to: 100` of three lines still answers `a.rs:2-3`.

---

## 5. Claims examined and **not** upheld as stated

Recorded so they are not re-litigated.

**5.1 — "`map_drill` on an empty file yields header `path:1-0`."** Half right, and the half that
is wrong matters. At the `rusta-repomap` level the claim reproduces exactly
(`drill(Window{from:1,to:5})` on an empty file → `Ok("empty.rs:1-0\n")`). But the **tool wrapper
rewrites the header**, and what actually reaches the model is `empty.rs:1-1`. Both name a line
that does not exist, so A34 stands as a defect — but a fix that only chases the string `1-0`
would leave the model-facing output untouched.

**5.2 — GLM: "No new High or Medium findings this round."** Contradicted by A26, which reproduces
by execution.

**5.3 — GLM: "every specified cap and fence that this audit could exercise by execution held"**
and **"no defects of the §16.1 recurring shapes … were found."** Both false at this commit: A26
and A32 are unfenced paths; A30 and A31 are inert safeguards. The report reached these
conclusions from reading, not from probing — the failure mode ADR §16.3 already documented for
the round-5 external review, repeating unchanged.

**5.4 — GLM as an independent second opinion.** It is not one. `RUSTA_AUDIT_REPORT_GLM.md` is the
round-7 report already committed to this repository at `30164b2` and removed at `63e1fb5`,
differing only by six blank lines and one fence label (` ``` ` → ` ```bash `). Its findings F1–F8
are genuine but were already on record; F2 (validator heuristics lack real-output fixtures) and
F4 (the embedded clippy graph was not re-run) remain valid and open.

**5.5 — Not hallucinated.** No finding in either report was fabricated. Every `file:line` citation
this round checked resolved to real code saying what the report said it said. The KIMI report's
disputed content is a single over-precise string (§5.1); the GLM report's problem is its
conclusion, not its facts.

---

## 6. Carried forward — raised but **not** verified this round

KIMI's report contains 29 Low findings and 47 explicitly quarantined risks (R-01–R-47). This
round independently verified the 1 High, all 6 Mediums, and 21 Lows. The remainder —
**F-19, F-20, F-21, F-22, F-25, F-26 and all of R-01–R-47** — were read but not reproduced, and
are recorded here as leads rather than findings, in keeping with §16.6 rule 3. They are not
included in this report's counts and should not be treated as confirmed.

The most substantive among them, worth investigating first because each names a specific
mechanism rather than a style concern: R-01 (unbounded SSE buffer), R-03 (`map_drill` reads whole
files while `read` streams), R-04 (blocking sync I/O on tokio workers), R-05 (detached dispatch
tasks are not cancelled), R-31 (cap markers appended past the cap in three more places — the same
shape as A36), and R-40 (Ctrl-C is not listened for during a running `dispatch`).

---

## 7. The pattern, and what would actually change it

Round 8 found no new *kind* of defect. It found the three ADR §16.1 shapes again, plus a fourth
that the ADR does not yet name: **the normative document drifting from the code it governs.**
Eleven of the twenty-one Lows (A39–A41, A46–A50, A52, A53) are statements in doc comments, test
docs, CI config or the ADR itself that the code does not support — including two miscounts in
`state.rs`, the file whose entire purpose is exactness, and one false claim in ADR §16.4 about
the completeness of the previous round's own remediation.

Three observations that follow from this round specifically:

1. **§16.2 was applied to two of three consumers.** The A10 fence went to `read` and `map_drill`
   and not to `grep`; the test that should have caught it is named for all three and exercises
   one. The rule is right; its application was not checked against the list §6.12 itself provides.
2. **"Pinned by a regression test" is now a claim that needs auditing, not a conclusion.** A31
   shows one such claim already false. The cheap structural answer is a test that asserts the
   mapping — every finding ID in ADR §16.4 must appear in some test name or comment — which would
   have caught A31 mechanically.
3. **Reading a report is not reviewing a codebase.** The strongest evidence this round is six
   probes totalling under 80 lines. They found the High finding, the two confinement leaks, the
   fabricated `read` range and the vanishing filename. The report that read instead of probing
   concluded PASS.

---

## 8. Suggested order

**Wave 1 — information flow.** A26 (`grep` fence, and extend the A10 test to all three tools) ·
A32 (map renderer fence). Both are the same one-line class of fix and should land together with
a single test that walks every read-side tool.

**Wave 2 — data integrity and dead guards.** A29 (`/undo` partial failure) · A28 (silent
truncation) · A30 + A31 (test the stream-failure path and the A8 bound) · A27 (deny table, or
amend §6.12).

**Wave 3 — model-facing truth.** A35 (fabricated `read` range) · A34 (empty-file drill range,
both layers) · A36 (clip marker) · A33 (non-ASCII filenames) · A42 (CRLF asymmetry) · A44
(fence prefix) · A43 (false undo message).

**Wave 4 — document truth.** A39–A41, A45–A53. Cheap individually; collectively they are the
finding, because ADR §16.4's false completeness claim is what a future round would rely on to
skip re-verification.

---

*Method note: every High and Medium above was verified at `63e1fb5` — A26, A32, A33, A34, A35 by
executing probes against the real tool registry, the remainder by reading the cited `file:line`.
Probes were removed after use; `git status` clean apart from the two source reports. Gates
re-executed this session and reported in the header.*
