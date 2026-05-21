---
name: send-message
description: Emit a plain-text reply with the send_message MCP tool, with rules for the `to` field, multi-line bodies, code blocks, and links.
---

# send-message

`send_message` is the primary outbound tool. It writes one row into the
session's `outbound.db` and lets the host's delivery loop hand it to the
appropriate channel adapter.

## Schema

```json
{ "to": "telegram:chat-123", "text": "string, non-empty" }
```

- `text` (required, non-blank). Whitespace-only text is rejected with
  `ToolError::Validation`.
- `to` (optional). Three accepted shapes:
  - String form: a fully-qualified channel id, e.g. `"telegram:chat-9"` or
    `"slack:C01AB23"`. Treated as `{ "kind": "channel", "id": ... }`.
  - Tagged channel: `{ "kind": "channel", "id": "telegram:chat-9" }`.
  - Tagged agent: `{ "kind": "agent", "session_id": "sess_abc" }`.
  - Tagged user: `{ "kind": "user", "id": "u_42" }` (host resolves the
    route via `user_dms`).

## When to omit `to`

Omit `to` whenever you are replying to the inbound message you are
processing. The runner reads `session_routing` and fills in the
originating `(channel_type, platform_id, thread_id)`. This is the right
default for almost every conversational reply.

Only set `to` when you are deliberately routing somewhere else:
- A different channel the agent is wired to (e.g. report a Telegram event
  in a Slack ops room).
- Another agent by session id (delivers as a `MessageKind::Agent` row,
  bypassing channel adapters).
- A user across all their known DMs.

## Multi-line bodies

`text` is a single string; embed `\n` for line breaks. Channels render
this in their native style: Telegram preserves newlines, Slack collapses
runs of blank lines, Discord respects markdown. Do not try to insert
platform-specific control sequences here — use `send_card` instead.

## Code blocks and links

The channel adapter is free to apply markdown. Most platforms recognise
triple-backtick fences for code and `[label](url)` for links. If the
channel needs HTML or a custom block format (e.g. Slack `mrkdwn` quirks),
the adapter rewrites at delivery time; you can write plain markdown.

## Examples

Reply on the origin channel:

```json
{ "text": "Done. The report is in your DMs." }
```

Forward to another agent:

```json
{ "to": { "kind": "agent", "session_id": "sess_7c2" },
  "text": "FYI: ticket #42 escalated." }
```

Reply with a fenced code block:

```json
{ "text": "Here is the diff:\n```diff\n- old\n+ new\n```" }
```

## Result

The tool returns an ack carrying the new outbound `seq` (an odd integer).
Save that seq if you intend to call `edit_message` or `add_reaction` on
the same message later.
