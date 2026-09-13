#!/usr/bin/env bash
# R1 enforcement gate — DEVELOPMENT_PLAN.md §12.
# Production budget: ≤ 15,000 lines; total (including tests): ≤ 20,000 lines.
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

split_counts() {
  find crates -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' -type f -print0 |
    xargs -0 awk '
      FNR == 1 { in_test = 0 }
      !in_test && /^[[:space:]]*#\[cfg\((all\()?test/ { in_test = 1 }
      { if (in_test) t++; else p++ }
      END { printf "%d %d\n", p, t }
    '
}

read -r PROD INLINE_TESTS < <(split_counts)
SUITE_TESTS=$(find crates \( -path '*/tests/*' -o -name 'tests.rs' \) -name '*.rs' -type f -exec cat {} + | wc -l)
TESTS=$((INLINE_TESTS + SUITE_TESTS))
TOTAL=$((PROD + TESTS))

echo "production=${PROD} tests=${TESTS} (inline=${INLINE_TESTS} suites=${SUITE_TESTS}) total=${TOTAL}"
[ "$PROD" -le 15000 ] || { echo "FAIL: production budget exceeded (${PROD} > 15000)"; exit 1; }
[ "$TOTAL" -le 20000 ] || { echo "FAIL: total budget exceeded (${TOTAL} > 20000)"; exit 1; }
