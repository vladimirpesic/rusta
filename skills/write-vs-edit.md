---
name: write-vs-edit
type: tool
triggers: [write, create_file]
priority: 4
token_cost: 80
user-invocable: false
---
# WRITE is for NEW files only. Change existing files with edit (SEARCH/REPLACE)

- read or map_drill the file first;
- write to an existing file requires a prior read this session;
- small exact edit blocks beat full-file rewrites.
