---
name: approvals
description: Pending approvals — sender, channel, saved-skill, and task-grant — and how an admin resolves them via the cclaw tool.
---

# approvals

Several operations in copperclaw require an admin's explicit consent
before the host acts on them. The flow is uniform: the requestor
writes an "approval" row; an admin (a user with the `admin` role)
inspects and either approves or denies; the host applies the
decision.

This skill covers the four kinds of approval an agent will encounter
and how a human resolves them via `cclaw`.

## The approval families

| Family | Triggered by | Row table |
|---|---|---|
| `Sender`      | unknown platform user sending into a wired channel | `unregistered_senders` |
| `Channel`     | first message arriving on a channel + platform id not yet known | `pending_channel_approvals` |
| `SaveSkill`   | `save_skill` MCP tool                              | `pending_approvals` |
| `TaskGrant`   | `schedule_task` called with a `grant`              | `pending_approvals` |

`pending_approvals` carries an `action` string and a `payload` JSON
blob describing the request. The other tables exist because senders and
channels need a richer per-row state (last-seen times, denial flags)
than a generic payload can carry. Note that `install_packages` and
`add_mcp_server` are NOT approval-gated today — the host applies them
directly to the group's container config at delivery time, and the
change lands at the next container spawn.

An unanswered approval lapses — rows default to a ~1 hour TTL, after
which the request is dead and must be re-raised.

## Sender approvals

When an inbound event arrives whose sender identity is not in
`users` and is not already approved, the router writes an
`unregistered_senders` row keyed by
`(channel_type, platform_id)` and returns `Pending`. The agent does
**not** process the message until the admin approves the sender via
`cclaw approvals approve --channel <ct> --identity <id>`, which
inserts a `users` row.

## Channel approvals

The first time the host sees a `(channel_type, platform_id)` pair
that no admin has wired, it writes a `pending_channel_approvals`
row. Until an admin acknowledges, the host treats the channel as
inert — no agent receives traffic from it.

## Skill / grant approvals

When you call `save_skill`, or `schedule_task` with a `grant`, the
host writes a `pending_approvals` row and raises an approval card to
the operator. Nothing is written or authorised until a human approves:
a saved skill lands on disk only on approval (see [[save-skill]]), and
a task grant authorises autonomous action only on approval (see
[[schedule-task]]).

## How an admin resolves an approval

```bash
cclaw approvals list                # list pending (all families)
cclaw approvals get <approval-id>   # show one
cclaw approvals approve-id <id>     # approve any family by row id
cclaw approvals deny <id>           # deny (no side effects, idempotent)
cclaw approvals revoke <id>         # withdraw a pending request
cclaw approvals decisions           # append-only decision audit log
cclaw approvals approve --channel <ct> --identity <id> [--display-name "Name"]
                                    # sender shortcut by (channel, identity)
```

Approve → sender becomes a `users` row / channel becomes a
`messaging_groups` row / the skill is written or the grant persisted.
Deny → sender or channel rows are marked denied (future sends from
that identity land in `dropped_messages`); skill / grant requests are
dropped (the agent may retry with a better reason).

## What the agent sees

The agent does not directly see approval rows. It learns about them
through the absence of an effect it expected (a saved skill that never
appears next session) or a `kind: system` message the host writes when
the admin acts — that's the right cue for "my request finally landed."

## Tips

- Always include a clear `reason` so the admin can decide quickly.
- Don't re-call `save_skill` (or re-attach a `grant`) in a tight loop
  — wait for the system-message ack or the next session.
- For high-stakes requests, `schedule_task` a follow-up to verify the
  approval ever landed and surface the result to a user if not.
