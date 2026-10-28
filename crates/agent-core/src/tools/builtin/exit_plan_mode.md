Present your plan for the user's review and, once they approve it, leave plan mode. Use this tool only in plan mode, which only the user can turn on, and only for a task whose implementation steps need planning. For research tasks (searching files, reading code, understanding the codebase), do NOT use this tool.

## How This Tool Works
- Pass the whole plan as `plan`, in Markdown.
- The user reviews the plan before plan mode ends, in every permission mode, and approves or rejects it. Once they approve it, plan mode is off and you carry the plan out.

## What a good plan contains
Write the plan for a reviewer: they should be able to judge it quickly, and an implementer should be able to carry it out without making further decisions. Keep it concise, usually in these sections:
- Context — why the change is needed and the outcome it should produce.
- Approach — the approach you recommend.
- Key changes — grouped by behavior or subsystem, not file by file. Name a file only when that makes a change unambiguous; for a change repeated across many files, describe the pattern once and list a few representative paths. Point to existing functions or utilities to reuse, with their paths.
- Verification — how to check the result end to end: the tests or commands to run and the scenario to exercise.
- Assumptions — each decision you made without the user's input, and the default you chose.
Avoid vague filler like "improve performance" or "add tests"; say what changes and how you will know it works.

## Before Using
- If questions about preferences or trade-offs are still open, use `ask_user_question` first. When nobody can answer, decide from context and list those decisions under Assumptions.
- If you have multiple approaches and haven't narrowed them down, consider using `ask_user_question` first to let the user choose, then write a plan for the chosen approach only.
- Do NOT use `ask_user_question` to ask "Is this plan OK?" or "Should I proceed?" - that is exactly what this tool does.
- If the user rejects the plan, find out what to change, revise the plan, and call this tool again.
