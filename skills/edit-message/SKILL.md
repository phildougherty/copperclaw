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
- CLI / stdio: prints the new text as a fresh line (best-effort
  fallback; the original is not retroactively rewritten).
- Webhook-only channels: typically unsupported, returns
  `AdapterError::Unsupported`.

When the channel cannot edit, the tool ack still succeeds (the row is
written) but the delivery loop surfaces an `Unsupported` failure that
gets retried until `MAX_DELIVERY_ATTEMPTS`. Treat persistent edit
failures as "channel does not support this" and stop retrying.

## Edge cases

- **Message already edited / deleted upstream.** Most platforms return
  4xx; the adapter maps to `AdapterError::BadRequest`. The host will
  not retry indefinitely (3-attempt cap).
- **Message older than the channel's edit window.** Treated the same
  as `BadRequest`.
- **Edit removed by moderation.** Same path. Do not panic; surface a
  user-facing apology via `send_message` if relevant.
- **You did not actually send the original.** The seq does not belong
  to this session. The tool will write the row but the host's
  delivery loop will fail to locate the original; the request is
  dropped.

## Example

```json
{ "message_id": 13, "text": "Update: build is green now." }
```
