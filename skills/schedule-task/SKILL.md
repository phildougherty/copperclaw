---
name: schedule-task
description: USE THIS for anything periodic or recurring — every N minutes, every N hours, daily, hourly, regular monitoring, future-dated one-shots, any "do this on a schedule" pattern. The schedule_task tool enqueues a prompt with a cron recurrence (use the string */30 * * * * for every 30 minutes) and the host re-invokes the agent when the task fires — there is no need for a persistent loop or background agent; the scheduler IS the loop. Companions list_tasks, cancel_task, pause_task, resume_task, update_task.
---

# schedule-task and friends

Scheduling tools enqueue prompts that the host re-injects into your
session at a configured time and/or cadence. When the task fires the
host writes a `MessageKind::Task` row into your `messages_in` table;
your next poll picks it up like any other message.

Six tools cover the lifecycle:

| Tool | Schema | Purpose |
|---|---|---|
| `schedule_task`  | `{ name, when?, prompt, recurrence?, grant? }` | enqueue |
| `list_tasks`     | `{}` | inventory current tasks |
| `cancel_task`    | `{ id }` | permanently remove |
| `pause_task`     | `{ id }` | stop firing, keep state |
| `resume_task`    | `{ id }` | re-arm a paused task |
| `update_task`    | `{ id, prompt?, when?, recurrence? }` | edit in place |

## `When` syntax

`schedule_task` requires **at least one of** `when` or `recurrence`.

- `when` is an RFC 3339 date-time, always interpreted as UTC. Example:
  `"2026-05-21T06:00:00Z"`. The agent's wall-clock interpretation is
  set at the host; if the agent's locale matters, format the value
  yourself.
- `recurrence` is a cron expression (croner dialect — standard 5-field
  form; a 6-field form with a leading seconds field is also accepted).
  Examples:
  - `"0 9 * * *"` — every day at 09:00 UTC.
  - `"*/15 * * * *"` — every 15 minutes.
  - `"0 9 * * 1-5"` — weekdays at 09:00 UTC.
- Friendly relative forms (`"in 5m"`, `"daily at 09:00"`) are accepted
  by the host's natural-language layer and normalised before the row
  is written; they are not part of the JSON schema you call directly
  with, but cclaw admins can use them.

If both `when` and `recurrence` are supplied, `when` is the first
fire time and `recurrence` controls everything after that.

## Example: one-shot

```json
{ "name": "morning-standup",
  "when": "2026-05-21T13:30:00Z",
  "prompt": "Summarise yesterday's commits across the open repos." }
```

## Example: cron

```json
{ "name": "hourly-poll",
  "prompt": "Check the deploy queue and post status if anything moved.",
  "recurrence": "0 * * * *" }
```

## Optional `grant`: pre-authorising autonomous action

By default a task's autonomous fires can only draft. Attach a `grant`
object when the task should ACT on its own (e.g. actually send the
standup message): `{ capability_scope, expires_at, reason,
token_budget?, max_fires? }`. `capability_scope` is space-separated
`class` or `class:resource` tokens (e.g. `"send_message:telegram"`);
`expires_at` is required and at most 365 days out; at least one of
`token_budget` / `max_fires` must be set (grants are bounded, never
unlimited). A grant is approval-gated — the task is created either
way, but the grant only takes effect after an operator approves it
(see [[approvals]]).

## Listing, pausing, resuming

`list_tasks` returns a JSON array of `TaskSummary` values:

```json
[ { "id": "task_018f9c2d-7e1a-7b4d-a2f6-1d2c3e4f5061",
    "name": "hourly-poll",
    "status": "active", "when": null,
    "recurrence": "0 * * * *" } ]
```

Task ids are `task_<uuidv7>` (always the full UUID — don't shorten them
when passing back to `cancel_task` / `pause_task` / etc.).

`status` is one of `active` / `paused` / `cancelled` / `completed`.
Use the `id` in `pause_task` / `resume_task` / `cancel_task` /
`update_task`.

## Updating in place

```json
{ "id": "task_8a", "prompt": "New text", "recurrence": "*/30 * * * *" }
```

Pass `null` for `when` or `recurrence` to **clear** that field (the
schema preserves a difference between "do not touch" and "set to
null"). At least one of `prompt` / `when` / `recurrence` must be
present.

```json
{ "id": "task_8a", "when": null }
```

…makes a previously one-shot task purely recurring (assuming a
recurrence is set) or, if no recurrence, idle.

## What you receive when a task fires

A `messages_in` row of kind `task`, with `content` containing the
`prompt` you registered. The `series_id` correlates all fires of a
recurring task; `recurrence` is propagated so you can recognise
"this is a scheduled run, not an interactive request."

## Notes

- Task ids are stable across restarts; they live in the central DB.
- A paused task does not fire; its recurrence "skips" while paused.
- Cancelling is irreversible. To re-create, call `schedule_task` again.
- An unparseable recurrence never fires — if a recurring task goes
  silent, `list_tasks` and check the expression.
