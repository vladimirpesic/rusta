# Rusta — Consolidated Audit Report

**Commit audited:** `b58eb41` (HEAD → main) · **Date:** 2026-09-15
**Normative baseline:** `DEVELOPMENT_PLAN.md` v1.2 including all recorded errata
**Sources consolidated:** `RUSTA_AUDIT_REPORT_GLM.md`, `RUSTA_AUDIT_REPORT_KIMI.md`, and the
fifth in-repo audit pass. Every finding below was **independently re-verified against the code
at `b58eb41`** before inclusion; findings that did not reproduce are listed in §5 rather than
silently dropped.

**Gate status at `b58eb41` (re-run):** `cargo fmt` clean · `clippy --all-targets -D warnings` 0
issues · `cargo doc` 0 warnings · `cargo test --workspace` **273 passed, 0 failed** ·
`--features rusta-cli/embedded` 285 passed, 3 GGUF ignored · LoC 11,817 / 19,503 against
15,000 / 20,000.

**Every finding in this report sits behind green gates.** That is the project's standing
condition, not a new one.

---

## 1. Verdict

**3 High · 8 Medium · 14 Low · 1 spec ambiguity.** No finding is remotely exploitable; R11
holds (the only network path is the user-configured backend). The three High findings are all
data-integrity: one writes marker text into user source, one bricks auto-resume for a
repo-day, and one makes `/undo` revert work it should not touch.

Three of the High/Medium findings are defects **in the fourth audit's own remediation**
(A2, A3-a, A4, A12) — the pattern this project keeps reproducing, and the reason §6 proposes
a change to how fixes are accepted rather than another list of fixes.

### Reconciling the two external reports

The two reports reach opposite verdicts on the same commit.

| | GLM | KIMI | This report |
| --- | --- | --- | --- |
| Verdict | "PASS — ship-quality", 0 Medium-or-above | 10 MAJOR, ~45 MINOR | 3 High, 8 Medium, 14 Low |
| Accuracy of cited facts | High — spot-checks confirmed | High — every MAJOR reproduced but one sub-claim | — |
| Main failure | **Accepted the guards' own claims about themselves** | Over-graded a few MINORs; one sub-claim wrong | — |

**GLM is not hallucinating.** Its specific citations check out: `write_file_is_confined_too`
exists at `apply.rs:1715`; `property.rs` really does assert the §6.12 containment invariant
under generated input (lines 148–168); its embedded test count (23) is exact. Its error is
different and more instructive — it verified that each guard *exists* and read what each guard
*says about itself*, then reported the guard's claim as the finding. Its headline assertion
that `write_paths.rs` "makes a fifth unfenced write path un-addable" is **false**, and is
disproved in A4 by adding five. Two minor factual slips: it says the corpus has 16 fixtures
(there are 15), and it credits `Backend::stop()` as closing the "flag had no caller" class
while missing that the guard it praises has the same defect one level up.

**KIMI is substantially correct.** All ten MAJOR findings reproduced. One sub-claim did not
(§5.1), and a few MINORs are graded above their impact, but its method — naming a file:line
and a reproduction for each — held up under re-verification at a rate no other source matched.

---

## 2. High

### A1 — A glued `>>>>>>> REPLACE` marker is written into user source (mid-stream)

*Origin: KIMI M1 · `crates/rusta-edit/src/parser.rs`*

§6.3 rule 4's erratum exists precisely to stop `>>>>>>> REPLACE` reaching a user's file.
`split_trailing_marker` is applied **only** in the end-of-stream branch. When a block is closed
mid-stream — by a following UPDATED line, or by a chained DIVIDER — `updated.concat()` is
committed with the marker fused to the last content line, and **no corrective note is
emitted**, so neither the user nor the model learns of it.

```bash
end-of-stream    updated="new\n"                      notes=1   ← fixed case
mid-stream (UPD) updated="new>>>>>>> REPLACE\n"        notes=0   ← written to the file
mid-stream (DIV) updated="new>>>>>>> REPLACE\n"        notes=0   ← written to the file
```

The apply is reported successful and auto-committed. Any completion containing more than one
edit block reaches this path.

**Fix:** run the strip on every commit out of `in_replace`, not only at end-of-stream.

### A2 — The first writes after a forgiven torn tail brick auto-resume

*Origin: KIMI M4 · `crates/rusta-core/src/session.rs` · **regression from the 4th audit***

The torn-tail forgiveness added in the fourth audit fixes *reading* and never repairs the file.
`Session::open` replays past the partial line, then appends through `O_APPEND` without
truncating it. The next `record()` fuses its JSON onto the unterminated tail.

One append is survivable (the fused line is still last, so forgiveness covers it, and exactly
one event is lost silently). **The second append is not** — the fused line is now mid-file:

```bash
{"type":"user_message","content":"hi"}
{"type":"assistant_message","cont{"type":"user_message","content":"second request"}
{"type":"session_end"}

REOPEN FAILED: corrupt session log … line 2: expected `:` … Remedy: …
```

Sessions auto-resume from `~/.rusta/sessions/<slug>-<UTC date>.jsonl`, so this is the exact
"refuses to start in that repo for the rest of the day" failure the erratum was written to
eliminate — re-created by the fix for it. (The remedy text added in the third audit does at
least tell the user how to recover.)

**Fix:** truncate the log — and the sidecar — to the consumed valid prefix after replay.

### A3 — `/undo`'s batch stack is reconstructed by three different broken rules

*Origin: 5th-pass H1, KIMI M7, KIMI M9 · `crates/rusta-cli/src/repl.rs`, `commands.rs`*

Three independent defects converge on one cause: **the undo journal and the batch stack are
rebuilt from different sources by different rules, and undo operations are not recorded at
all.** Each is separately reproducible.

**(a) Skipped edits desynchronise the count (regression from the 4th audit).** The fourth
audit's fix drops workspace-escaping paths from the journal. `rebuild_batches` was not told, and
still counts every `EditApplied`. A batch then claims more entries than it holds, and
`for _ in 0..batch.entries` walks past the batch boundary:

```bash
batches=[1, 2]   journal depth=2     (batch 2 claims 2, holds 1)
$ /undo                              one press
restored 2 file(s)
  y.txt   ← batch 2, correct
  x.txt   ← batch 1, a different commit
```

The loop only stops on an *empty* journal, so the over-consumption is invisible whenever
anything older remains.

**(b) No-git mode merges unrelated batches (KIMI M9).** `rebuild_batches` starts a new batch
only when the previous one has `sha.is_some()`. With no git repo — a shipped, documented
degradation path — or after a failed commit, consecutive batches from *separate user requests*
merge into one, and a single `/undo` pops all of them.

**(c) `/undo` journals nothing (KIMI M7).** `commands.rs::undo` records no session event.
Replay therefore resurrects entries for edits already undone. Apply A→B, `/undo` back to A,
edit A→C, restart: a later `/undo` writes **B over C**, silently rewinding the working tree to
content the user already rejected, and reports it as "restored 1 file(s)".

**Fix:** one source of truth. `rebuild_batches` must apply the same `confine` filter replay
does; batch boundaries must come from an event (a `UserMessage` boundary or an explicit
marker) rather than from `Commit`; and `/undo` must journal a tombstone that replay honours.

---

## 3. Medium

### A4 — The structural write-path guard detects fewer than half the mutation primitives

*Origin: 5th-pass H2/H3 · `crates/rusta-edit/tests/write_paths.rs` · **regression from the 4th audit***

`write_paths.rs` was the fourth audit's structural answer to three rounds of the same defect.
Its `MUTATORS` list covers four primitives. Five unfenced mutations were added outside
`guarded` and the test stayed **green**:

```rust
fs::File::create(&abs)?   +  f.write_all(...)      // uncovered
fs::rename(&abs, ...)?                             // uncovered
fs::copy(..., &abs)?                               // uncovered
fs::create_dir(...)?          // create_dir_all is listed; create_dir is not
$ cargo test --test write_paths → ok. 1 passed
```

Also uncovered: `OpenOptions`, `fs::set_permissions`, `fs::hard_link`, `symlink`. The claim is
made **twice in the source** — `apply.rs:47` ("A new write path cannot be added unfenced
without turning that test red") and `write_paths.rs:7` — and is false in both places. GLM
repeated it as a verified strength.

Three further weaknesses in the same scanner: it has **no proof of life** (nothing asserts it
found the calls it should find, so a rename or a desync passes vacuously); `read_dir` is
non-recursive, so a future submodule is never scanned; and brace tracking is textual, so one
unbalanced brace in a string literal desyncs it — silently, in the direction that suppresses
findings.

*Mitigating:* an independent scan confirmed the **funnel itself is complete** — the only
mutation outside `rusta-edit` in all eight crates is `create_dir_all` for the session directory
under `~/.rusta/`, deliberately outside the repo. Only the detector is incomplete.

**Fix:** complete the primitive list, and assert the detector's own proof of life.

### A5 — §6.5's user-mention boosts are dead in production

*Origin: KIMI M8 · `rusta-tools/src/map.rs:74`, `rusta-tools/src/dispatch.rs`, `rusta-cli/src/commands.rs:231`*

§6.5 steps 3–4 specify ×10 edge weight for a user-mentioned identifier and `+100/N`
personalization for mentioned files. **Every production caller of `render_map` passes
`&[], &[]`.** Only unit tests of `boost_multiplier` exercise those paths. This is the same
class as the withdrawn `strict_grammar` key: a specified, implemented, tested mechanism that
no production path can reach.

**Fix:** extract identifiers from the latest user message (the repomap's own word-scan is
sufficient) and thread them through — or amend §6.5.

### A6 — §6.8's sub-coder transcripts are never persisted

*Origin: KIMI M6 · `crates/rusta-dispatch/src/actor.rs`, `rusta-core/src/session.rs`*

§6.8: "the full sub-transcript goes to the session log only — never main context."
`Report.transcript` is built and documented as session-log material. The §6.10 schema defines
**no transcript-bearing event**, and no production code reads the field; `labeled()` strips it
and the transcript is dropped. The audit trail the spec promises does not exist.

**Fix:** add a transcript sidecar/event, or correct §6.8 and the doc comments.

### A7 — `/resume` duplicates every dispatch report

*Origin: KIMI M5 · `crates/rusta-core/src/session.rs`*

The report replays twice — once inside the `dispatch` `ToolResult` observation, once again as
the standalone `Dispatch` event:

```bash
[User] research
[User] TOOL RESULT dispatch (ok) / SUB-CODER "a" REPORT: / findings
[User] SUB-CODER "a" REPORT: / findings          ← the same report, again
```

This breaks the byte-for-byte replay fidelity that M3/M5 claim and that
`Compressor::apply_summary` sharing exists to guarantee. No test pins replay-with-dispatch.

**Fix:** reconstruct from exactly one source (the `Dispatch` events are the better one).

### A8 — Unbounded allocation from a server-supplied `tool_calls` index

*Origin: KIMI M2 · `crates/rusta-llm/src/http.rs:373-377`*

```rust
let index = call.index as usize;
if index >= tools.len() { tools.resize(index + 1, ToolCallAccumulator::default()); }
```

`index` is a wire-supplied `u32` with no bound. `ToolCallAccumulator` is ~72 bytes, so
`"index": 4294967295` requests roughly **309 GB** in one contiguous allocation — an abort, not
a recoverable error, against §6.1's "never panics on malformed input" posture. Requires a
malicious or buggy backend, which R11's threat model treats as user-configured and trusted;
that lowers severity but does not make an abort an acceptable response to a malformed field.

**Fix:** cap the index (≤ 64 is generous) and return `Error::Malformed`.

### A9 — §6.12's shell environment allow-list does not exist

**Origin: 5th-pass H4**:

§6.12 specifies "minimal environment (PATH, HOME, LANG **+ config allow-list**)". There is no
such allow-list: no `env` key in §7's schema, none in `config.rs`, none in
`rusta.toml.example`. `[shell] allow` is a list of *command prefixes that skip approval* — a
different feature with a confusingly similar name.

Not a breakage — a standard rustup toolchain runs fine on the minimal environment (verified:
cargo 1.98.1, rustc, git all succeed) — which is why five audits missed it. It is a gap: a
project whose validators need `RUSTFLAGS`, `CARGO_TARGET_DIR`, `DATABASE_URL` or a proxy
variable has no way to supply one.

### A10 — Read-path tools follow in-repo symlinks out of the workspace

*Origin: GLM F-3, KIMI 3.4(d), 3rd-pass F13 (read half) · `rusta-tools/src/exec.rs:93-115`*

`safe_rel` is lexical: it rejects absolute paths and `..`, but a committed symlink
`notes → /home/user/secrets` resolves through `fs::read` for `read`, `grep` and `map_drill` —
and `map_drill` credits the ledger for it. All **mutation** paths are fenced by
`contains_path`, so this is information flow only: outside-repo content enters the model
context and the session log. Requires a hostile repo. §6.12 governs mutations; the read side
is ungoverned by the plan.

### A11 — The main session log is never re-tightened to `0600`

*Origin: GLM F-1, KIMI 3.3(b) · `crates/rusta-core/src/session.rs:162`*

`restrict_existing` exists because "`OpenOptions::mode` applies only when the file is created"
and old logs "are exactly the logs still in use" — its own doc comment. It is called **once**,
at line 244, for the *sidecar*. `Session::open` never calls it for the main log, so a log
created before the guard (or under a looser umask) stays group/world-readable for the whole
session while being appended to. The smaller file is protected; the larger is not.

**Fix:** one line — `restrict_existing(&path)` in `Session::open` after `replay()`.

---

## 4. Low

| # | Finding | Location | Evidence |
| --- | --- | --- | --- |
| **A12** | `map_drill`'s rewritten header is off by one in the byte-cap path — the truncation marker is counted as a content line. *Regression from the 3rd audit's "honest header" fix.* | `rusta-tools/src/map.rs` | header `long.rs:1-164`, last real line `163` |
| **A13** | `grep` clips matching lines at 200 chars with **no marker**, against §6.1's "append a marker whenever a cap truncated" | `rusta-tools/src/search.rs:114` | clipped line 211 chars, no marker |
| **A14** | `/add` chat-set membership does not survive `/resume` — replay credits the ledger only from `read`/`map_drill` results and `EditApplied`; `/add` journals a `"session"`-named observation | `commands.rs:112` vs `session.rs:332` | inspection |
| **A15** | Sub-coder report clip marker is not charged against the 400-token cap — shipped reports run ~411 tokens | `rusta-dispatch/src/actor.rs:217-228` | `take(400*3)` then append |
| **A16** | Unstartable validator report carries **no remedy**, so a missing cwd burns all three repair rounds; the sibling timeout path has one (§6.11) | `rusta-validate/src/validators.rs:347` | inspection |
| **A17** | Stream-open and mid-stream failures fire `PhaseEvent::UserInterrupt`, journaling `reason: "user interrupt"` — a transient network error is misattributed and discards plan state | `rusta-cli/src/agent.rs:580, 614` | inspection |
| **A18** | Dead `CONTEXT_WINDOW_FRACTION: f64 = 0.6` beside the live integer `window * 3 / 5` — an edit-one-not-the-other trap | `rusta-core/src/context.rs:45` | only doc-comment references |
| **A19** | The fuzz alphabet's doc claims CRLF coverage it does not have — no `\r\n` piece exists, so the "CRLF normalized" invariant is vacuous for generated input | `rusta-edit/tests/property.rs:34` | inspection |
| **A20** | `read` loads the **whole file** into memory before any cap applies — same memory class as the 3rd audit's shell finding | `rusta-tools/src/read.rs:27` | inspection |
| **A21** | The context-overflow guard reports to the user but neither aborts the turn nor tells the model; the oversized request still ships | `rusta-cli/src/agent.rs:565` | inspection |
| **A22** | The `rusta-full` artifact named in `README:77` and `rusta-cli/Cargo.toml:19` **does not exist** — one `[[bin]] name = "rusta"`, no release workflow distinguishes them | `Cargo.toml:14` | inspection |
| **A23** | §13's "GGUF manual test matrix documented" is unmet — `RUSTA_TEST_GGUF` appears only in the plan and a test module doc, never in a user-facing document | `README.md` | inspection |
| **A24** | Session write errors are swallowed at 15 sites (`let _ = …record(…)`); a full disk silently stops journaling and `/resume` loses the unrecorded portion | `rusta-cli/src/{agent,repl,commands}.rs` | 12 + 2 + 1 occurrences |
| **A25** | `capped_read` treats a pipe read error as clean EOF, so a truncated stream is indistinguishable from a complete one | `rusta-core/src/proc.rs` | inspection |

**Spec ambiguity (not a defect).** §6.1 lists "dispatch 400 tokens" among per-tool-result
caps; §6.8 says 400 per *report*. `labeled()` applies no joint cap, so four sub-coders produce
~1,600 tokens plus labels in one observation. The two readings differ 4×; the plan should pick
one.

---

## 5. Claims examined and **not** upheld

Recorded so they are not re-litigated.

**5.1 — KIMI: "`read` can exceed the 64 KiB byte cap by one full line."** Not reproduced. The
loop increments `bytes_used` *before* the bound check, so the overshooting line is never
pushed. Measured 65,313 bytes against the 65,536 cap. The *other* half of that finding — the
whole-file `fs::read` before caps — is valid and is recorded as A20.

**5.2 — GLM: "`write_paths.rs` … makes a fifth unfenced write path un-addable."** False;
disproved in A4 by adding five. This is the load-bearing claim of GLM's "PASS" verdict.

**5.3 — GLM: "the corpus-agreement test drives all 16 fixtures."** There are 15
(`tests/edit_corpus/01`–`15`). Cosmetic.

**5.4 — GLM: overall verdict "0 Critical, 0 High, 0 Medium."** Contradicted by A1, A2 and A3,
each independently reproduced.

**Spot-checks that *confirmed* GLM's accuracy** (i.e. not hallucinations): the cited test
`write_file_is_confined_too` exists at `apply.rs:1715`; `property.rs` does assert the §6.12
containment invariant under generated input; the embedded suite is exactly 23 lib tests; the
Aider repomap and apply-chain parity claims hold — all seven `.scm` queries are byte-identical
to the current reference, and the edge-weight ladder matches `repomap.py:485-514`.

---

## 6. The pattern, and what would actually change it

Five audits have found the same three shapes: **an unfenced path**, **an inert safeguard**, and
**a test green on an input production never supplies.** Each round fixed its instances; each
round's fix introduced new ones. A2, A3-a, A4 and A12 are all defects in the previous round's
remediation.

The fourth round tried to break this structurally: stop enumerating write paths by hand, make a
test do the counting. That was the right instinct, and A4 is why it failed — **the guard was
verified only against the case that motivated it.** `write_paths.rs` was confirmed red against
the seven calls that already existed and green after they moved. That proves it can see *those*
calls and nothing about the ones it exists to catch. Testing a detector means introducing what
it should catch; that took one probe and would have caught A4 before the commit.

Three changes would do more than another fix list:

1. **A detector must assert its own proof of life** — that it found what it expects to find, so
   it cannot pass by not looking.
2. **A fix that changes what a data structure contains must be checked against every consumer
   of that structure.** A3-a is a second consumer of the undo journal that nobody looked for
   when entries started being dropped from it.
3. **Reject "the guard says so" as evidence.** GLM's verdict is the cautionary case: every
   citation accurate, the conclusion wrong, because guards were read rather than tested.

**R1 is effectively spent: 19,503 of 20,000 total lines.** Fixing this report will not fit.
That decision — raise the cap, budget tests separately, or move work to opt-in crates per §0
rule 5 — now precedes the remediation rather than following it.

---

## 7. Suggested order

**Wave 1 — data integrity.** A1 (marker into source) · A2 (bricked resume) · A3 (all three
`/undo` defects together, since they share one cause).

**Wave 2 — guards and dead features.** A4 (complete the detector *and* give it proof of life) ·
A5 (mention boosts) · A6 (transcripts) · A7 (dispatch replay) · A8 (index bound) · A11
(one line).

**Wave 3 — spec truth and hygiene.** A9 · A10 · A12–A25, plus the §6.1/§6.8 dispatch-cap
ambiguity and the §4 per-crate targets, which §7 describes as enforced while only the global
totals are gated.

---

*Method: every finding re-verified against `b58eb41` by execution or direct code inspection
before inclusion; reproductions shown inline. Temporary probes removed; working tree clean.
Reference parity re-checked against `/home/vladimir/develop/refs/aider`.*
