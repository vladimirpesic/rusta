---
name: locate-the-cause
type: recovery
triggers: [test_failure, patch_target_not_found]
priority: 9
token_cost: 85
user-invocable: false
---
# FIX THE CAUSE, NOT THE TEST

A failing test states expected behaviour. It is evidence, not the bug.

- Never edit a test to match current output — that hides the defect.
- The bug is in the code the test calls. Read that function.
- Trace one failing case by hand until the step that diverges.
- Two tests failing together usually share one cause.
