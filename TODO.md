# TODO — §14 out-of-tree benchmark harness

**Status:** not started · **Written:** 2026-09-21 · **Next session:** another machine.

Self-contained on purpose: it assumes no memory of the conversation that produced it, and no
knowledge of the machine it was written on. Everything it asserts about timing was measured;
everything it guesses says so.

**Delete this file when the harness exists**, folding anything decided along the way into
`ADR.md` §14. That is this project's convention (`fd42f87`, and the LSP work that folded its own
plan into §14.1 and deleted its TODO).

---

## 1. Why this exists

Ten audit rounds have produced a codebase nobody can evaluate. Rounds 9 and 10 added nine
mechanisms — a stream guard, a whole-file fallback, four detectors, a failure bar, a bigger skill
deck — each justified by a reference implementation and by a logged failure, and **not one of
them is measured.** ADR §16.10 states this plainly. Until a harness exists:

- "superior to the reference projects" is unfalsifiable;
- a regression in small-model effectiveness is invisible, because the 350-test suite pins
  behaviour and says nothing about outcomes;
- the next round repeats the pattern of rounds 9 and 10, where the *first real run* found a
  defect in the round that had just been completed.

**Out-of-tree is a requirement, not a preference.** ADR §4 fences an in-tree benchmark out of
scope and §14 parks it; §12 counts every `*.rs` under the workspace against R1. Keep it in a
sibling directory or its own repository. Nothing here is added to `crates/`.

## 2. Prerequisites — verify before building anything

```sh
rustc --version                  # 1.98.1 on the authoring machine; 1.88+ required by mcpls-core
cargo --version
git --version
python3 --version                # the runner may be python or bash; pick one and say so
ollama list                      # or: curl -s http://127.0.0.1:11434/v1/models
command -v rust-analyzer         # only if benchmarking the `lsp` feature
free -h                          # see §5: model choice is a memory decision
nproc                            # and a throughput one
```

Build the binary under test, and record which feature graph:

```sh
cargo build --release                              # default graph
cargo build --release --features rusta-cli/lsp     # with LSP enrichment
```

**Do not skip recording the commit.** A benchmark number without `git rev-parse HEAD` is not a
measurement. Baselines below are from `2202aec`.

## 3. Build Tier 1 first — the regression suite

Cheapest, and the piece that would have caught the defects rounds 9 and 10 shipped.

**Scope:** 5–6 tasks, all drawn from failures already observed, each with a seeded bug, tests
that fail before and pass after, and a known-good fix committed on a branch for reference.

| Task | Shape | Known result |
| --- | --- | --- |
| `sum_even` off-by-one | 1 file, 1 line, `0..limit` should be `0..=limit` | 7B **solved** it (2026-09-15) |
| RPN operand order | 3 files, 2 failing tests, 1 cause — `lhs`/`rhs` popped in the wrong order | 30B **solved** it in 6 min; 7B failed three times |
| Malformed tool call | prompt that induces a non-JSON ```tool fence | exercises the §6.6 repair path |
| Invented SEARCH text | bug whose fix tempts a from-memory SEARCH | exercises the whole-file fallback |
| Repeated failing call | task where one tool call is easy to get wrong | exercises the failure bar |
| Read-only question | "what does `evaluate` do?" — no edit expected | must exit 0 and apply nothing |

The last row matters: it is the control. A harness that only measures "did it edit" will reward a
model for editing when it should not.

**Runner, per task:**

1. `git -C <fixture> reset --hard <seed-commit> && git clean -fd`
2. Confirm the tests **fail** — a fixture that passes before the run measures nothing.
3. Run with an explicit, isolated session file:

   ```sh
   timeout 2700 rusta --session "$OUT/$task.jsonl" -c "$PROMPT"
   ```

   Record the exit code. **It is now meaningful**: non-zero means edits were offered and none
   changed a file, or the final validators were red (ADR §6.9's 2026-09-20 revision).
4. Run the fixture's tests again; that is the pass/fail.
5. Extract metrics from the session JSONL (§4).

**Two traps, both hit on the authoring machine:**

- **Sessions auto-resume by `<slug>-<date>`.** Two runs in the same fixture on the same day share
  context, and the second inherits the first's failures. One run confounded an entire comparison
  that way. Always pass `--session` with a unique path.
- **Always rebuild before a run.** A stale `target/release/rusta` silently benchmarks an old
  commit.

## 4. Metrics — parse the session JSONL

One JSON object per line, `{"type": ...}` snake_case (ADR §6.10). Tags observed in real runs:

```bash
session_start  user_message  assistant_message  tool_call  tool_result
edit_applied   validation_run  state_change  batch_boundary  commit
dispatch  session_end
```

Worth recording per task, because each has already explained a failure:

| Metric | From | Why |
| --- | --- | --- |
| passed | fixture tests after the run | the outcome |
| wall clock, exit code | the runner | exit code now encodes false success |
| turns | count `assistant_message` | turn-cap pressure |
| tool calls, by name | `tool_call.name` | 32 of 34 being one tool is a loop |
| distinct vs repeated call args | `tool_call.input`, canonicalised | how the failure bar was found |
| edits offered vs applied | `edit_applied`, and `before_hash == after_hash` | **a no-op edit is not progress** |
| phase arc | `state_change` from/to + reason | a run that never leaves `exploring` is stuck |
| `loop escalation` reason | `state_change.reason` | which detectors fired |
| commits | `commit` | did a batch actually land |

Emit one CSV or JSON row per task and keep the raw JSONL — the interesting findings this week all
came from re-reading raw logs, never from the summary.

## 5. Model matrix and what it costs

Measured on the authoring machine (i7-8700, 6 physical cores, CPU-only, 31 GiB, ZFS):

| Model | Load | Per task | Notes |
| --- | --- | --- | --- |
| `qwen2.5-coder:7b` (4.7 GB) | seconds | 8–15 min | fits alongside an editor |
| `qwen3-coder:30b-a3b-q4_K_M` (18 GB) | ~30 s | ~6 min solved, 45 min timeout | needs ~20 GB |

Both were wrapped in derived models pinning `num_ctx` (see §6), and both ran CPU-only at roughly
97%/3% CPU/GPU.

**Budget the machine time, not the coding time.** 20 tasks × 2 models ≈ **7–10 h per pass**.
Aider's real Polyglot set is 225 exercises — about **37 h per model** here, which is why Tier 3
is a different-hardware problem, not a scheduling one.

If the new machine has a GPU with ≥ 24 GB, re-measure before trusting any of this: the whole
table changes and Tier 3 may become reachable.

## 6. Ollama setup — the one that silently corrupts results

Ollama truncates from the **left** past its `num_ctx`, discarding the system prompt and every
phase gate with it, and reports nothing. `rusta.toml`'s `[model] context_window` must equal the
model's `num_ctx` or the benchmark measures a scaffold that is not there.

Pin it with a derived model rather than trusting a server default:

```sh
printf 'FROM qwen3-coder:30b-a3b-q4_K_M\nPARAMETER num_ctx 16384\nPARAMETER num_thread 6\nPARAMETER temperature 0.2\n' > Modelfile.30b
ollama create bench-qwen30b -f Modelfile.30b
ollama show bench-qwen30b | grep -i num_ctx     # verify, do not assume
```

`num_thread` is **physical** cores. Matching `rusta.toml`:

```toml
[backend]
kind = "http"
base_url = "http://127.0.0.1:11434/v1"   # full API root incl. /v1 (§6.2)

[model]
name = "bench-qwen30b"
context_window = 16384                   # MUST equal num_ctx above
max_tokens = 2048
temperature = 0.2

[agent]
max_turns = 16
auto_approve = false                     # `-c` auto-approves the plan gate anyway

[validate]
commands = ["cargo test"]
timeout_secs = 600
```

## 7. Tier 2 — the comparative suite

Only after Tier 1 runs clean twice.

- 15–25 tasks: Exercism gives "implement from scratch" free (`git clone
  https://github.com/exercism/rust`, as Aider's `benchmark/clone-exercism.sh` does), but Rusta's
  claim is bug-fixing and multi-file localisation, so hand-authored bug tasks carry the weight.
  Aim for a mix and label which is which — they measure different things.
- Vary one axis at a time: model, `lsp` on/off, `[lsp] deadline_ms`, `max_turns`.
- **Report error bars or do not report a comparison.** At 20 tasks one flip is 5%. To separate
  "round 10 helped" from noise, repeat runs — which multiplies the machine time in §5. Decide up
  front whether you are measuring an effect or sanity-checking for regressions; they need
  different suite sizes and the second is usually what you actually want.

## 8. Reference harnesses worth reading first

In `/home/vladimir/develop/refs/` on the authoring machine (re-clone if absent):

- `aider/benchmark/benchmark.py` (1,059 lines) — per-exercise isolation, retries, summarisation;
  `clone-exercism.sh` is the corpus; `Dockerfile` is how they get reproducibility.
- `smallcode/bench/harness.js` (639 lines) + `bench/README.md` — smoke / polyglot / tool-use
  suites, and the closest sizing to what is wanted here.
- `SmallCTL/evals/` — `tool_plan` and `test_time_scaling`, closer to per-mechanism evaluation
  than end-to-end scoring.

## 9. Standing caveats

- **The first run will find harness bugs.** It has on every first run in this project: the LSP
  feature, the phase gate, the stream guard, and a mock that only spoke SSE and therefore never
  exercised `complete()` at all. Budget a second pass before trusting any number.
- **A benchmark measures the harness too.** If Rusta scores badly, rule out the runner before
  concluding anything about the scaffold — rounds 9 and 10 both began with a "model failure"
  that turned out to be Rusta's.
- **Do not tune against the suite.** Once a number exists it is tempting to optimise for it. Keep
  a held-out task or two that never informs a change.
- **The 7B is the honest target.** It is where every §16.10 mechanism aims, and where the
  measured failures are. A suite only the 30B can pass will not tell you whether the scaffold
  works.
