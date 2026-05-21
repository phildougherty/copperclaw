---
name: destinations
description: Route outbound messages with the `to` field — channel destinations, agent destinations, and the named-destination registry.
---

# destinations

When you emit an outbound message, the `to` field decides where it
goes. This skill explains the shapes the field accepts, the
named-destination registry, and how the host resolves a `to` value to
a concrete delivery target.

## The three forms of `to`

In the JSON sent to a tool, `to` is one of:

- **Bare string**. A fully-qualified channel id understood by the
  channel router. Example: `"telegram:chat-9"`, `"slack:C01ABCD"`.
  The runner treats this as `{ "kind": "channel", "id": "..." }`.
- **Tagged channel**. Same as above, in the explicit form:
  `{ "kind": "channel", "id": "telegram:chat-9" }`.
- **Tagged agent**. `{ "kind": "agent", "session_id": "sess_…" }`.
  Routes via the host's inter-agent path, not a channel adapter. The
  outbound row carries `MessageKind::Agent`.
- **Tagged user**. `{ "kind": "user", "id": "u_42" }`. The host
  looks up the user's DM destination across all known channels
  (`user_dms` table) and picks one.

Omitting `to` is the most common case: the runner fills it from
`session_routing` (the channel the inbound that woke this turn arrived
on). Use that unless you mean to send somewhere else.

## Channel destinations vs agent destinations

- Channel: the message goes through a `ChannelAdapter::deliver` call.
  The platform delivers to the user; you may get a platform message id
  back, which the host stores in `delivered.platform_message_id`.
- Agent: the message is written to the destination session's
  `messages_in.db` with `kind = "agent"`. The destination agent reads
  it on its next turn. There is no platform; the host is the relay.

You can mix freely — call `send_message(to=Channel{...})` and
`send_message(to=Agent{...})` in the same turn.

## Named destinations

Each agent group has a `destinations` table (per-session DB,
populated from the central wiring). Rows let you address a
destination by a short name in tools that surface it. The row shape:

```text
name            display_name    kind     channel_type  platform_id  agent_group_id
"ops"           "Ops Channel"   channel  slack         C01OPS
"bossbot"       "Senior Bot"    agent                              <UUID>
```

In future tool calls (planned but not in the v0 surface) the `to`
field will accept a bare name like `"ops"` and the runner will
resolve it via this table. For now, use the channel id or agent
session directly.

## How resolution works

For a tagged channel id like `"slack:C01ABCD"`:

1. The runner splits at the first colon: `("slack", "C01ABCD")`.
2. The host looks up the channel adapter for `"slack"` in the
   `ChannelRegistry`.
3. The adapter's `deliver` method takes `platform_id = "C01ABCD"` and
   `thread_id = None` (unless the originating row had one).

For a tagged agent destination:

1. The destination session id is parsed.
2. The host opens the destination session's `inbound.db`.
3. A row is written with `kind = "agent"` and `source_session_id`
   set to your session.

## Common patterns

- **Reply on the originating channel**: omit `to`.
- **Cross-post to a fixed channel**: use a tagged channel id.
- **Hand off to a sibling agent**: use a tagged agent.
- **DM a user globally**: use a tagged user; the host picks the
  channel from `user_dms`.

## Errors

- Empty / whitespace `id` or `session_id` → `ToolError::Validation`.
- Unknown channel type at delivery time → `AdapterError::Unsupported`,
  surfaced via the delivery loop's retry/back-off path.
- Unknown user id at delivery time → host writes a
  `dropped_messages` row and moves on.
