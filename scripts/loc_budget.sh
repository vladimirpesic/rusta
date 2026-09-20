#!/usr/bin/env bash
# R1 enforcement gate — ADR.md §12.
# Production budget: ≤ 15,000 lines; total (including tests): ≤ 30,000 lines.
#
# The total cap was 20,000 and was reached by the fifth remediation round.
# Production is not the pressure — it sits at ~12.3k of its unchanged 15k.
# All of the growth is *tests*: five audits' worth of regression coverage,
# which is the one thing this project's defect history says it needs most.
# Holding the old total would have meant deleting tests to stay green, the
# precise incentive §12's own erratum was written to remove, so the total
# was raised deliberately rather than paid for out of coverage.
# Canonical counting is this script (`wc -l` on `*.rs`); tokei is an optional
# cross-check. Data files (.scm queries, skills/*.md, prompt fragments) are never counted.
#
# Unit tests count as *tests*, wherever they live. The original script
# excluded only `tests.rs` and `tests/` directories, but this workspace puts
# its unit tests in a trailing `#[cfg(test)] mod tests` inside each source
# file — so ~4k lines of tests were being charged to the production budget,
# inflating it by roughly a third and creating pressure to delete tests to
# stay under the cap. Each source file is therefore split at its `#[cfg(test)]`
# marker: everything above is production, everything from it down is test.
set -euo pipefail

# A48: §12 says the budget is "counted on `*.rs`" with no `crates/` boundary,
# but the scan only ever looked there — a workspace-root `tests/`, `benches/`
# or `examples/` escaped R1 entirely. Harmless when found (root `tests/` holds
# only the `.md` edit corpus), but a gate whose scope is narrower than its
# contract is the shape §12's own erratum was written about.
ROOTS="crates"
for extra in tests benches examples; do
  [ -d "$extra" ] && ROOTS="$ROOTS $extra"
done

split_counts() {
  # `xargs` may split a long file list across several awk invocations, each
  # printing its own totals — so sum the partial lines rather than reading
  # only the first. (At ~50 files this never splits today; a gate that
  # silently undercounts as the workspace grows is exactly the failure mode
  # §12's erratum was written about.)
  find $ROOTS -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' -type f -print0 |
    xargs -0 awk '
      FNR == 1 { in_test = 0 }
      !in_test && /^[[:space:]]*#\[cfg\((all\()?test/ { in_test = 1 }
      { if (in_test) t++; else p++ }
      END { printf "%d %d\n", p, t }
    ' |
    awk '{ p += $1; t += $2 } END { printf "%d %d\n", p, t }'
}

read -r PROD INLINE_TESTS < <(split_counts)
SUITE_TESTS=$(find $ROOTS \( -path '*/tests/*' -o -name 'tests.rs' \) -name '*.rs' -type f -exec cat {} + | wc -l)
TESTS=$((INLINE_TESTS + SUITE_TESTS))
TOTAL=$((PROD + TESTS))

echo "production=${PROD} tests=${TESTS} (inline=${INLINE_TESTS} suites=${SUITE_TESTS}) total=${TOTAL}"
[ "$PROD" -le 15000 ] || { echo "FAIL: production budget exceeded (${PROD} > 15000)"; exit 1; }
[ "$TOTAL" -le 30000 ] || { echo "FAIL: total budget exceeded (${TOTAL} > 30000)"; exit 1; }
