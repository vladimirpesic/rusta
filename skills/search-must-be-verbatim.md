---
name: search-must-be-verbatim
type: recovery
triggers: [patch_target_not_found]
priority: 9
token_cost: 80
user-invocable: false
---
# SEARCH DID NOT MATCH — copy, do not compose

SEARCH is matched literally. Reconstructed text will not match.

1. `read` those lines again, or `map_drill` the name.
2. Copy verbatim: indentation, quotes, commas.
3. Add a unique neighbouring line if the snippet repeats.

After two failures on one file, use `write` instead.
