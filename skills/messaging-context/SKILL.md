---
name: messaging-context
description: Read inbound messages — kinds, sender identity, mention vs DM, thread state — so the agent reacts to the right thing.
---

# messaging-context

Every turn, the runner reads pending rows from
`inbound.db.messages_in`, formats them, and hands the batch to the
provider. Each row carries everything you need to react correctly.

## Message kinds

The `kind` field is one of:

| Kind | Source | Treat as |
|---|---|---|
| `chat`    | user typed on a channel | conversational |
| `task`    | scheduled task fired   | self-issued prompt |
| `webhook` | external service POSTed  | event to process |
| `system`  | host-internal synthetic  | admin / CLI / ack |
| `agent`   | another agent sent you   | inter-agent |

`kind` is a lowercase string. Branch on it before assuming the row is
conversational.

## Sender identity

Chat rows carry a sender block:

```json
{
  "sender": {
    "channel_type": "telegram",
    "identity": "@alice",
    "display_name": "Alice Liddell"
  },
  "text": "hi"
}
```

- `channel_type` + `identity` = unique platform user key. The host
  resolves to a `UserId` (UUIDv5 deterministic) before storing.
- `display_name` is best-effort.
- Inter-agent messages have no human sender; the source `session_id`
  is on the row's `source_session_id`.

## Mentions vs DMs vs group chat

Each row has:

- `is_mention` — platform message explicitly addressed your agent
  (`<@U…>` for Slack, `@yourbot` for Telegram, role/user mention for
  Discord).
- `is_group` — arrived in a group/channel rather than 1-1 DM.

| is_group | is_mention | Meaning |
|---|---|---|
| `false` | n/a | DM. Always respond. |
| `true`  | `true`  | Group ping. Respond. |
| `true`  | `false` | Background chatter. Respect wiring's `engage` (pattern, mention, mention-sticky). |

`mention-sticky`: once you reply in a thread, the host treats next
non-mention messages there as addressed to you until idle. Trust the
row's `is_mention` unless you have strong reason to override.

## Threading

`thread_id` set when the platform exposes threads (Slack, Telegram
topic groups, Discord threads). `None` for:

- Channels without thread support.
- Top-level messages in a thread-capable channel.

When replying with `send_message`, omit `to`; the runner copies the
inbound `thread_id` into the outbound row, keeping the conversation
in-thread.

## `content` shape per kind

- `chat`: `{ "text": "...", "sender": {...} }` + optional `files`.
- `task`: the prompt you registered with `schedule_task` + `recurrence`
  (so you recognise the scheduled fire) + `series_id` (correlates
  fires).
- `webhook`: the verbatim JSON the external service POSTed, under
  `body`.
- `system`: `{ "kind": "<sub-kind>", ... }`. Sub-kinds:
  `cli_request`, `cli_response`, ack payloads.
- `agent`: same shape as `chat` but with `source_session_id` set and
  no `sender.channel_type`.

## Attachments

Files extract to `inbox/<msg_id>/<filename>`. The path mounts into the
container read-only. Read with normal filesystem APIs; do not fetch
from the platform.

The runner runs `safe_attachment_name()` before extraction. Files
with `..`, `/`, leading dots, or length > 255 are dropped.

## Worked example

A Slack mention in a thread:

```json
{
  "kind": "chat",
  "channel_type": "slack",
  "platform_id": "C01XYZ",
  "thread_id": "1714578122.000200",
  "content": {
    "text": "<@U_agent> can you take a look?",
    "sender": { "channel_type": "slack", "identity": "U_alice",
                "display_name": "Alice" },
    "is_mention": true,
    "is_group": true
  }
}
```

Reply with `send_message({"text": "On it."})` and no `to` — the
runner fills in `(slack, C01XYZ, 1714578122.000200)` from
`session_routing`.
