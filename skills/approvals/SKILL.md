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

## The four approval kinds

| Kind | Triggered by | Row table |
|---|---|---|
| `Sender`           | unknown platform user sending into a wired channel | `pending_sender_approvals` |
| `Channel`          | first message arriving on a channel + platform id not yet known | `pending_channel_approvals` |
| `InstallPackages`  | `install_packages` MCP tool                          | `pending_approvals` |
| `AddMcpServer`     | `add_mcp_server` MCP tool                            | `pending_approvals` |
| `OneCli`           | first OneCLI device link                             | `pending_approvals` |

`pending_approvals` carries a typed `kind` and a `payload` JSON blob
describing the request. The other two tables exist because senders
and channels need a richer per-row state (last-seen times, denial
flags) than a generic payload can carry.

## Sender approvals

When an inbound event arrives whose sender identity is not in
`users` and is not already approved or denied, the router writes a
`pending_sender_approvals` row keyed by
`(messaging_group_id, sender_identity)`. The agent does **not**
process the message until the admin approves the sender.

The router still records the sender in `unregistered_senders` so
the admin can see who is knocking (and how often).

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

Today, `iclaw` exposes the read side:

```bash
iclaw approvals list                # list all pending
iclaw approvals get <approval-id>   # show one
```

The write side (approve / deny) is driven through the central DB
update functions exposed in `ironclaw-db::pending_approvals` and
the related per-kind tables. In a real deployment, the admin uses
a higher-level CLI (`iclaw approvals approve <id>` / `deny <id>`)
that translates to the appropriate table update.

When the admin approves:

- For sender approvals: a `users` row is created (or attached);
  any future inbound from that sender flows.
- For channel approvals: a `messaging_groups` row is created;
  wirings can be added.
- For install / MCP approvals: the `container_configs` row is
  updated and the container is queued for rebuild.

When the admin denies:

- Sender approvals: the row is marked denied; future sends from
  that identity land in `dropped_messages`.
- Channel approvals: same — the channel is parked.
- Install / MCP approvals: the request is dropped; the agent
  may try again with a better reason.

## What the agent sees

The agent does not directly see approval rows. It learns about
them only through:

- The absence of an action it expected (`install_packages` did
  not lead to a new package on the next boot — admin hasn't
  approved yet).
- A system message of kind `system` containing an approval
  outcome, written by the host when the admin acts. This is
  the right place to detect "my install is finally live."

## Practical tips

- Always include a `reason` that helps the admin decide quickly.
- After requesting an install or MCP server, do not call
  `install_packages` again in a tight loop — wait for the next
  boot or for a system message acknowledgement.
- For high-stakes operations, schedule a follow-up
  (`schedule_task`) to check whether the approval ever
  landed; if not, surface the situation to a user.
