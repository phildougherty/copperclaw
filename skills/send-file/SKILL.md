---
name: send-file
description: Attach a file with the send_file MCP tool, including base64 encoding rules, when to inline text instead, and filename safety.
---

# send-file

`send_file` writes a binary attachment to the session outbox and lets the
host deliver it via the appropriate channel adapter. The file is staged
under `outbox/<msg_id>/<filename>` and dispatched alongside the outbound
row.

## Schema

```json
{
  "to": "telegram:chat-123",
  "filename": "report.pdf",
  "data": "<base64 bytes>",
  "text": "optional caption"
}
```

- `filename` (required, non-blank). The host re-validates with
  `safe_attachment_name()`: no `..`, no `/`, no leading dot, length
  bounded at 255. A failing name is bounced before delivery.
- `data` (required, non-empty). Base64-encoded bytes. The tool decodes
  on the way in; an invalid base64 payload returns
  `ToolError::Validation`.
- `text` (optional). A caption shown beside the file on channels that
  support it (Telegram caption, Slack `initial_comment`). Channels
  without inline captions ignore it.
- `to` accepts the same forms as `send_message` (string, tagged channel,
  tagged agent, tagged user).

## When to use `send_file` vs inline text

Use `send_file` when:
- You produced a binary artifact (image, PDF, archive).
- The text is longer than a few screens and a downloadable file is
  cleaner than spamming a thread.
- The recipient needs to forward the artifact intact.

Prefer `send_message` with a triple-backtick code block when:
- The payload is short (under ~2 KB) and meant to be read inline.
- The channel renders markdown well (most do).
- The recipient is another agent (agents read text faster than they
  unpack attachments).

## Attachment limits

The tool itself does not impose an upper bound; channel adapters do.
Practical ceilings observed in current adapters:

- Telegram: 50 MB for non-bot files, 20 MB for bots.
- Slack: 1 GB per file (uses upload session); inline previews capped
  much lower.
- Discord: 25 MB on free guilds.

If you suspect you are near a limit, send a download link via
`send_message` instead.

## Multiple files in one message

A single tool call carries one file. To attach several to the same
logical reply, call `send_file` once per file in order. They land as
separate outbound rows but share routing context.

## Example

```json
{
  "filename": "metrics.csv",
  "data": "Zm9vLGJhcgoxLDIK",
  "text": "Latest counts (Mon-Fri)."
}
```

This will appear in the recipient's chat as `metrics.csv` with the
caption "Latest counts (Mon-Fri).". The tool returns an ack with the
outbound `seq`.
