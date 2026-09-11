---
name: edit-recovery
type: recovery
triggers: [edit_failed, not_found, duplicate_match]
priority: 9
token_cost: 95
user-invocable: false
---
# EDIT FAILED — recover, never rewrite wholesale

- not found: re-read the file, copy SEARCH exactly (indentation, quotes); add 2 context lines.
- multiple matches: widen SEARCH with one unique neighboring line.
- never fall back to write for an existing file; fix SEARCH and retry once.
