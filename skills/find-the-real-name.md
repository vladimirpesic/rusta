---
name: find-the-real-name
type: recovery
triggers: [wrong_path, not_found]
priority: 9
token_cost: 90
user-invocable: false
---
# THAT PATH OR NAME DOES NOT EXIST

Do not guess a second spelling — confirm it.

- `glob` for the file: `**/*.rs`, or a fragment you are sure of.
- `map_refresh` lists the definitions that actually exist, with their files.
- `grep` for the identifier to find where it is defined and used.

Guessing again costs a turn and usually fails the same way.
