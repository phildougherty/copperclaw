---
name: edit-message
description: Edit a previously sent message with the edit_message MCP tool, identifying messages by their outbound sequence number.
---

# edit-message

`edit_message` replaces the body of a message you already sent. The
operation is routed through the same channel adapter as the original
delivery; if the adapter declines, the host records the failure and
leaves the original intact.

## Schema

```json
{ "message_id": 7, "text": "new body" }
```

- `message_id` (required, integer > 0). This is the outbound `seq` you
  received in the ack for `send_message` / `send_file` / `send_card`.
  It is **not** the platform-side message id, and **not** a UUID.
  Sequences are odd (container-side writes); the host's inbound writes
  are even.
- `text` (required, non-blank). Total replacement; there is no patch
  syntax.

## What "message_id" refers to

When `send_message` (or any of the other outbound tools) returns, the
ack carries `{ "seq": <int> }`. That integer is the row's primary
ordering in `outbound.db.messages_out`. Persist it in your working
memory if you intend to edit later:

```text
ack = send_message({"text": "Working on it…"})
# later
edit_message({"message_id": ack.seq, "text": "Done. Report attached."})
```

## Channels without edit support

Not every channel can edit a delivered message. Behaviour by channel:

- Telegram: supports `editMessageText` (and `editMessageCaption` for
  media). Time-limited on some chat types.
- Slack: supports `chat.update` indefinitely on most channel types.
- Discord: supports `PATCH /channels/{id}/messages/{id}` indefinitely
  on the author's own messages.
- CLI / webhook-only channels: no native edit API.

When the channel cannot edit, nothing fails: the delivery loop falls
back to posting a fresh chat line `(edit) <new text>` and marks the
row delivered. The original message is not rewritten. You do not need
to detect or retry anything.

## Edge cases

- **Message already edited / deleted upstream.** Most platforms return
  4xx; the adapter maps to `AdapterError::BadRequest`. The host will
  not retry indefinitely (3-attempt cap).
- **Message older than the channel's edit window.** Treated the same
  as `BadRequest`.
- **Edit removed by moderation.** Same path. Do not panic; surface a
  user-facing apology via `send_message` if relevant.
- **You did not actually send the original.** No outbound row with that
  seq exists (or it was never delivered), so the delivery loop cannot
  locate a platform message id; it falls back to posting the text as a
  fresh `(edit) …` chat line.

## Example

```json
{ "message_id": 13, "text": "Update: build is green now." }
```

Prefer editing over posting a fresh status message when updating
something you already said — see [[native-ui]]. For reacting instead of
rewriting, see [[add-reaction]].
