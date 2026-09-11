---
name: read-large-files
type: tool
triggers: [read]
priority: 3
token_cost: 75
user-invocable: false
---
READ WISELY:
- use from/to to read 100-200-line slices, never whole files;
- for one symbol, prefer map_drill(path, name): it returns just the definition;
- read a file once; quote from context instead of re-reading.
