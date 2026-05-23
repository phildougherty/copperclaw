---
name: approvals
description: Pending approvals — sender, channel, install, and MCP — and how an admin resolves them via the iclaw tool.
---

# approvals

Several operations in ironclaw require an admin's explicit consent
before the host acts on them. The flow is uniform: the requestor
writes an "approval" row; an admin (a user with the `admin` role)
inspects and either approves or denies; the host applies the
decision.

This skill covers the four kinds of approval an agent will encounter
and how a human resolves them via `iclaw`.

## The approval families

| Family | Triggered by | Row table |
|---|---|---|
| `Sender`           | unknown platform user sending into a wired channel | `unregistered_senders` |
| `Channel`          | first message arriving on a channel + platform id not yet known | `pending_channel_approvals` |
| `InstallPackages`  | `install_packages` MCP tool                          | `pending_approvals` |
| `AddMcpServer`     | `add_mcp_server` MCP tool                            | `pending_approvals` |

`pending_approvals` carries an `action` string and a `payload` JSON
blob describing the request. The other tables exist because senders and
channels need a richer per-row state (last-seen times, denial flags)
than a generic payload can carry.

## Sender approvals

When an inbound event arrives whose sender identity is not in
`users` and is not already approved, the router writes an
`unregistered_senders` row keyed by
`(channel_type, platform_id)` and returns `Pending`. The agent does
**not** process the message until the admin approves the sender via
`iclaw approvals approve --channel <ct> --identity <id>`, which
inserts a `users` row.

## Channel approvals

The first time the host sees a `(channel_type, platform_id)` pair
that no admin has wired, it writes a `pending_channel_approvals`
row. Until an admin acknowledges, the host treats the channel as
inert — no agent receives traffic from it.

## Install / MCP approvals

When you call `install_packages` or `add_mcp_server`, the runner
writes a `pending_approvals` row. The row carries the requesting
session id, the agent group, the request payload, and a timestamp.

## How an admin resolves an approval

`iclaw` exposes read everywhere, write only for sender approvals:

```bash
iclaw approvals list                                       # list pending (all families)
iclaw approvals get <approval-id>                          # show one
iclaw approvals approve --channel <ct> --identity <id> [--display-name "Name"]
                                                           # approves a Sender row
```

There is no generic `iclaw approvals approve <id>` / `deny <id>` today.
Channel / InstallPackages / AddMcpServer approvals are resolved by the
operator either via the underlying CRUD on the central DB
(`ironclaw-db::pending_approvals` and the per-kind tables) or by
re-running the action through whatever workflow registered the row.

Approve → sender becomes a `users` row / channel becomes a
`messaging_groups` row / install or MCP request is applied and the
container is queued for rebuild. Deny → sender or channel rows are
marked denied (future sends from that identity land in
`dropped_messages`); install / MCP requests are dropped (the agent
may retry with a better reason).

## What the agent sees

The agent does not directly see approval rows. It learns about them
through the absence of an action it expected (an install that didn't
take effect at the next boot) or a `kind: system` message the host
writes when the admin acts — that's the right cue for "my install is
finally live."

## Tips

- Always include a clear `reason` so the admin can decide quickly.
- Don't re-call `install_packages` / `add_mcp_server` in a tight loop
  — wait for the next boot or the system-message ack.
- For high-stakes requests, `schedule_task` a follow-up to verify the
  approval ever landed and surface the result to a user if not.
