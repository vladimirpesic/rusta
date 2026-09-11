---
name: verify-focus
type: knowledge
triggers: [validation_failed, test_failure]
priority: 7
token_cost: 95
user-invocable: false
---
# VALIDATOR RED — work the first error, not all of them

- read the first failing line; map_drill the failing symbol;
- fix ONE narrow cause, then re-run only the failing validator;
- the same failing output twice means your edit missed the cause.
