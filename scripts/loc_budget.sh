#!/usr/bin/env bash
# R1 enforcement gate — DEVELOPMENT_PLAN.md §12.
# Production budget: ≤ 15,000 lines; total (including tests): ≤ 20,000 lines.
# Canonical counting is this script (`wc -l` on `*.rs`); tokei is an optional
# cross-check. Data files (.scm queries, skills/*.md, prompt fragments) are never counted.
set -euo pipefail
PROD=$(find crates -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' -type f -exec cat {} + | wc -l)
TESTS=$(find crates \( -path '*/tests/*' -o -name 'tests.rs' \) -name '*.rs' -type f -exec cat {} + | wc -l)
TOTAL=$((PROD + TESTS))
echo "production=${PROD} tests=${TESTS} total=${TOTAL}"
[ "$PROD" -le 15000 ] || { echo "FAIL: production budget exceeded (>$PROD)"; exit 1; }
[ "$TOTAL" -le 20000 ] || { echo "FAIL: total budget exceeded (>$TOTAL)"; exit 1; }
