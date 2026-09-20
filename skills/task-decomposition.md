---
name: task-decomposition
type: knowledge
triggers: [fix, bug, failing, broken, implement, refactor]
priority: 8
token_cost: 95
user-invocable: true
---
# DECOMPOSE FIRST — one unknown at a time

Before any tool call, write:

GIVEN: what the request already tells you.
UNKNOWN: 1–2 things to find out. More than 2 means split the task.
PLAN:
1. <tool action>
2. <tool action>
3. <the edit>

Resolve one UNKNOWN fully before the next. After each result, ask whether it
resolved one; if not, revise the PLAN.
