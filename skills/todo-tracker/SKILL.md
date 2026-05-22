---
name: todo-tracker
description: Keep a per-session scratchpad of multi-step work via `todo_add`, `todo_list`, `todo_update`, `todo_delete`. Use whenever a request needs more than two distinct steps to complete; reach for it for *any* kind of work, coding or otherwise.
---

# todo-tracker

A scratchpad you control. Four tools — `todo_add`, `todo_list`,
`todo_update`, `todo_delete` — back a JSON file at
`/data/agent_todos.json` inside your container. Items survive runner
restarts within the same session but never cross between sessions.

This is for *your* benefit, not the user's. The user doesn't see your
todos. You read them at the top of a turn so you don't forget where you
are.

## When to reach for it

- A user message implies 3+ discrete steps to satisfy ("look up X, then
  reply with a summary, then schedule a follow-up").
- A multi-turn workload where you might lose track of which subtasks are
  done if you only kept the state in conversation context.
- Any time you catch yourself about to write "step 1: ... step 2: ..."
  in a reply — that list belongs in `todo_add`, not in your prose.

## When NOT to reach for it

- One-shot answers ("what time is it?"). Adding a todo and then
  immediately completing it is wasted ceremony.
- Conversational chitchat. The user doesn't need a todo list for a
  greeting.
- Storing user-facing reminders. That's what `schedule_task` is for —
  it fires later, on its own.

## How to use

1. **At the start of a turn that needs planning:** add one todo per
   step with `todo_add({"text": "<short imperative>"})`. Keep each text
   line tight (one verb, one object): "Reply with order status",
   "Schedule follow-up for tomorrow".
2. **Before starting an item:** flip it to `in_progress` with
   `todo_update({"id": N, "status": "in_progress"})`. Only one item
   should be `in_progress` at a time.
3. **When you finish an item:** flip it to `completed`. Don't batch — do
   it as soon as the work for that step is done.
4. **If a step turned out unnecessary:** `todo_delete({"id": N})`.
   Don't leave dead items behind to clutter the list.

## Returned shape

Each entry looks like:

```json
{
  "id": 3,
  "text": "Reply with order status",
  "status": "pending",          // or "in_progress" / "completed"
  "created_at": "2026-05-22T14:08:01.103Z",
  "updated_at": "2026-05-22T14:08:01.103Z"
}
```

Ids are monotonic but sparse (deleted ids are not reused).

## Common pitfalls

- **Don't track every tool call.** A todo represents a *step* — a unit
  of work that might take several tool calls to satisfy. "Reply to
  user" is a step; "call `send_message`" is not.
- **Don't leave items stuck on `in_progress` across turns.** If you
  switched contexts, either complete it or flip it back to `pending`
  with a clarifying text edit.
- **Don't echo the todo list back to the user.** It's your scratchpad.
  The user reads your reply, not your bookkeeping.
