Cancel a scheduled cron job by id.

Use this tool to remove a cron task previously scheduled with
`cron_create`. The `id` is the value returned by `cron_create`, or
shown in the `id:` field of `cron_list` — quote it verbatim, no
prefix.

Behaviour by task kind:

- **Recurring task** (`recurring: true`): stops all future fires
  immediately. The scheduler picks up the deletion on its next tick.
- **One-shot task** (`recurring: false`): cancels the pending fire if
  it has not happened yet. One-shots that have already fired
  auto-delete themselves, so calling `cron_delete` on a fired one-shot
  returns "no cron job with id ...".

Not-found is reported as an error (not a silent no-op) so you can
correct yourself — typically by calling `cron_list` to see which ids
are actually live, rather than re-trying with the same stale id.

Refresh pattern (use when you want a stale recurring schedule to
continue):

Stale recurring tasks are auto-deleted by the system after their final
fire — there is nothing for `cron_delete` to remove at that point. To
keep the schedule running, just call `cron_create` with the same `cron`
and `prompt`. Use `cron_list`'s `prompt` field to recall the original
text after a context compaction.

`cron_delete` remains the right call when you want to cancel a task
that is still live (recurring not yet stale, or a one-shot still
pending).

Guidelines:

- Users have no direct `/cron` command or self-service UI to delete
  tasks themselves; they must ask the model to cancel a reminder.
  When deleting on behalf of a user, confirm the action and report
  the result plainly.
- Cron deletion is irreversible — there is no undo. If you delete the
  wrong task, you must re-create it with `cron_create`.
- If the model is unsure which id is current (e.g. after a context
  compaction), call `cron_list` first rather than guessing.
