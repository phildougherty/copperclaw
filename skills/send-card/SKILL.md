---
name: send-card
description: Emit a portable structured Card — rendered natively on Telegram (inline_keyboard), Slack (Block Kit), and Discord (embeds + components), with formatted-text fallback on every other channel.
---

# send-card

`send_card` emits ONE portable Card. The host renders it natively where
the adapter supports it (Telegram, Slack, Discord today) and falls back
to deterministic formatted text elsewhere — the same card works on every
channel.

## Schema

```json
{ "to": "telegram:chat-123", "card": { ...Card } }
```

- `to` (optional). Same forms as `send_message`. Omit for origin.
- `card` (required, object). Canonical fields:
  - `title` — optional, ≤ 256 chars.
  - `body` — optional, ≤ 4000 chars. Plain text or markdown.
  - `fields` — optional, ≤ 25. Each `{label, value, inline?}`. `label`
    non-empty ≤ 64 chars, `value` ≤ 1024 chars. `inline: true` hints
    side-by-side layout where supported.
  - `buttons` — optional, ≤ 8. Each `{label, value?, url?, style?}`.
    `label` non-empty ≤ 64 chars. Exactly one of `value` or `url`.
    `value` ≤ 64 bytes; tapping routes back as inbound chat carrying
    the value. `url` is http/https. `style`: `"primary"|"danger"|
    "secondary"`; ignored elsewhere.
  - `image_url` — optional, http/https only.

## Validation rules

Rejected before delivery if: no `title`/`body`/`fields`/`image_url`
(buttons alone aren't enough context); a button has neither or both of
`value`/`url`; any limit exceeded, label blank, or URL not http(s).
The validator names the offending field.

## Per-channel rendering

| Channel | Rendering |
|---------|-----------|
| Telegram | Native: MarkdownV2 + `inline_keyboard`. `image_url` rides as `sendPhoto` caption. |
| Slack | Native Block Kit: `header` / `section` / `image` / `actions`. `value` buttons honour `style: primary | danger`. |
| Discord | Native embed + `ActionRow`s of `Button`s (chunked at 5/row). `value` styles map primary→1, success→3, danger→4, else→2; URL→5. |
| All others | Text fallback: bold title, body, `Label: value` rows, then `[Label] -> callback:value` / `-> url` lines. |

On Telegram, Slack, and Discord, `value` buttons round-trip taps back as
inbound chat (see Callback flow). `url` buttons always open the link.

## Callback flow

When a user taps a `value` button the adapter writes an inbound chat
row whose content IS the button's `value`. Your next turn sees it as
if the user typed that value — no wait-for-callback tool. URL buttons
don't callback; they just open the browser.

## Examples

Confirmation (two `value` buttons):

```json
{ "card": {
    "title": "Approve deploy?",
    "body":  "Push the green build to prod-canary?",
    "buttons": [
      { "label": "Yes", "value": "deploy:yes", "style": "primary" },
      { "label": "No",  "value": "deploy:no",  "style": "danger" }
] } }
```

Preview with URL button:

```json
{ "card": {
    "title": "PR #318 ready",
    "body":  "Refactors runner outbox; +212 / -88.",
    "buttons": [{ "label": "Open PR",
                  "url": "https://github.com/o/r/pull/318" }]
} }
```

Status with fields and image:

```json
{ "card": {
    "title": "Build #1042 green",
    "body":  "All checks passed in 4m 12s.",
    "fields": [
      { "label": "Branch", "value": "main",    "inline": true },
      { "label": "Tests",  "value": "5214 ok", "inline": true }
    ],
    "image_url": "https://ci.example.com/1042.png"
} }
```

## When to use vs `send_message`

- `send_card`: choices ("Pick one"), structured info (status reports,
  comparisons), sticky actions a user might tap repeatedly.
- `send_message`: conversational prose. A card with no buttons/fields
  is usually a `send_message` in disguise.

## Anti-patterns

- No "Close" / "Dismiss" buttons — Telegram messages don't disappear;
  ack with a follow-up `send_message`.
- Don't set `value` identical to `label` unless that's genuinely what
  you want the next inbound to read as.
- Don't send a buttons-only card — validation rejects it.
- Don't pack platform JSON (Block Kit, Discord embeds) into `card`;
  use canonical fields — adapters translate.

## Result

Returns an ack with the outbound `seq` — save it for later
`edit_message` / `add_reaction`. Card JSON is stored in
`messages_out.content`.
