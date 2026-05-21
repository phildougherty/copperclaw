---
name: messaging-context
description: Read inbound messages — kinds, sender identity, mention vs DM, thread state — so the agent reacts to the right thing.
---

# messaging-context

Every turn, the runner reads pending rows from `inbound.db.messages_in`,
formats them, and hands the batch to the provider. The body of each
row carries everything you need to react correctly. This skill explains
how to read it.

## Message kinds

The `kind` field on every inbound row is one of:

| Kind | Source | Treat as |
|---|---|---|
| `chat`    | a user typed on a channel | conversational message |
| `task`    | a scheduled task fired   | self-issued prompt |
| `webhook` | external service POSTed  | event you must process |
| `system`  | host-internal synthetic  | admin / CLI / ack payload |
| `agent`   | another agent sent you   | inter-agent message |

The on-disk `kind` is a lowercase string (`"chat"`, `"task"`, etc.).
Always branch on it before assuming the row is conversational.

## Sender identity

Chat rows carry a sender block in `content`:

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

- `channel_type` + `identity` is the unique platform user key. The
  host resolves it to a `UserId` (UUIDv5 deterministic) before storing
  in central DB.
- `display_name` is best-effort — channels may not provide one.
- Inter-agent messages carry no human sender; instead they include the
  source `session_id` in the row's `source_session_id` field.

## Mentions vs DMs vs group chat

Each row has two booleans:

- `is_mention` — true when the inbound platform message explicitly
  addressed your agent (the channel adapter computed this; for
  Slack, `<@U…>`; for Telegram, `@yourbot`; for Discord, a role or
  user mention).
- `is_group` — true when the message arrived in a group / channel
  rather than a 1-1 DM.

Decision matrix:

| is_group | is_mention | Likely meaning |
|---|---|---|
| `false` | n/a | DM. Always respond. |
| `true`  | `true`  | Group ping. Respond. |
| `true`  | `false` | Background group chatter. Respect the wiring's `engage` mode (pattern, mention, mention-sticky). |

If the wiring's engage mode is `mention-sticky`, once you have replied
in a thread the host treats the next non-mention messages there as
addressed to you until the thread idles. Trust the row's `is_mention`
unless you have a strong reason to override.

## Threading

`thread_id` is set when the platform exposes threads (Slack threads,
Telegram topic groups, Discord threads). It is `None` for:

- Channels without thread support.
- Top-level messages in a channel that does support threads.

When replying with `send_message`, omit `to` and the runner copies the
inbound `thread_id` into the outbound row, keeping the conversation
in-thread.

## `content` shape per kind

- `chat`: `{ "text": "...", "sender": { ... } }` plus any
  attachments under `files`.
- `task`: the prompt you registered with `schedule_task`. Also
  includes `recurrence` (so you can recognise "this is a scheduled
  fire") and `series_id` (correlates fires of the same task).
- `webhook`: the verbatim JSON the external service POSTed,
  namespaced under `body`.
- `system`: `{ "kind": "<sub-kind>", ... }`. Sub-kinds include
  `cli_request`, `cli_response`, and ack payloads.
- `agent`: same shape as `chat` but with `source_session_id` set and
  no `sender.channel_type`.

## Reading attachments

When a chat row carries files, they are extracted to
`inbox/<msg_id>/<filename>`. The path is mounted into the container
read-only. Read with normal filesystem APIs; do not try to fetch them
from the platform.

The runner runs `safe_attachment_name()` on every file before
extraction. Files with `..`, `/`, leading dots, or length > 255 are
dropped before they reach you.

## Worked example

A Slack mention in a thread looks like:

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

Reply by calling `send_message({"text": "On it."})` with no `to` —
the runner fills in `(slack, C01XYZ, 1714578122.000200)` from
`session_routing`.
