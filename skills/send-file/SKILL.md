---
name: send-file
description: Attach a file with the send_file MCP tool — path vs base64 data, when to inline text instead, and filename safety.
---

# send-file

`send_file` writes a binary attachment to the session outbox and lets the
host deliver it via the appropriate channel adapter. The file is staged
under `outbox/<msg_id>/<filename>` and dispatched alongside the outbound
row.

## Schema

Preferred — the file is already on disk (you wrote it with `write_file`
or a build step produced it):

```json
{
  "to": "telegram:chat-123",
  "path": "/data/report.pdf",
  "text": "optional caption"
}
```

Only for bytes generated in-memory that you cannot save first:

```json
{ "filename": "report.pdf", "data": "<base64 bytes>" }
```

- `path` XOR `data` — exactly one, never both. **Never base64-encode a
  file that is on disk** and pass it as `data`: that overflows the
  model's `max_tokens` mid-tool-call. Use `path`; the tool reads the
  bytes itself. The `path` branch caps at 32 MB.
- `filename` — required with `data`; optional with `path` (defaults to
  the path's basename). The host re-validates with
  `safe_attachment_name()`: no `..`, no `/`, no leading dot, length
  bounded at 255. A failing name is bounced before delivery.
- `data` — base64-encoded bytes, non-empty. Invalid base64 returns
  `ToolError::Validation`.
- `text` (optional). A caption shown beside the file on channels that
  support it (Telegram caption, Slack `initial_comment`). Channels
  without inline captions ignore it.
- `to` accepts the same forms as `send_message` (string, tagged channel,
  tagged agent, tagged user) — see [[destinations]]. Omit to reply on
  the originating channel.

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

The `path` branch caps at 32 MB; channel adapters cap lower.
Practical ceilings observed in current adapters:

- Telegram: 50 MB for non-bot files, 20 MB for bots.
- Slack: 1 GB per file (uses upload session); inline previews capped
  much lower.
- Discord: 25 MB on free guilds.

If you suspect you are near a limit, send a download link via
`send_message` instead.

## Delivering a whole project (the build hand-off)

For a multi-file build, don't `send_file` each file — ship one archive.
Build it with `git archive` so build junk is excluded, then send it:

```bash
git -C /data/<project> archive --format=zip -o /data/<project>.zip HEAD
```

`git archive HEAD` naturally omits `.git` and anything gitignored (e.g.
`node_modules`), so commit first to make HEAD current. Pair the zip with
the `artifact_path` host path (for desk users) and, when the app serves
HTTP, an `expose_preview` link — see [[coding-task]] and [[preview]].
That file-plus-path-plus-link trio is the "prototype ready" hand-off.

## Multiple files in one message

A single tool call carries one file. To attach several to the same
logical reply, call `send_file` once per file in order. They land as
separate outbound rows but share routing context.

## Example

```json
{
  "path": "/data/metrics.csv",
  "text": "Latest counts (Mon-Fri)."
}
```

This will appear in the recipient's chat as `metrics.csv` with the
caption "Latest counts (Mon-Fri).". The tool returns an ack with the
outbound `seq` — save it if you plan to `edit_message` / `add_reaction`
later (see [[edit-message]], [[add-reaction]]).
