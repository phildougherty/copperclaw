---
name: add-reaction
description: React to a previously sent message with add_reaction, including per-channel emoji syntax differences.
---

# add-reaction

`add_reaction` attaches an emoji reaction to a message you have already
sent. Like `edit_message`, it identifies the target by outbound `seq`,
not by platform id.

## Schema

```json
{ "message_id": 7, "emoji": "thumbsup" }
```

- `message_id` (required, integer > 0). The outbound `seq` returned in
  the ack of the prior `send_*` call.
- `emoji` (required, non-blank). The shape of this string is
  channel-specific (see below).

## Emoji syntax per channel

The tool layer does not validate the emoji content — the channel
adapter does. Each platform expects a different format:

- **Slack**: short name with no colons, e.g. `"thumbsup"`,
  `"heavy_check_mark"`. The adapter calls `reactions.add` with this
  value verbatim. Custom workspace emoji are valid (`":my-custom:"`
  in Slack speak becomes `"my-custom"` here).
- **Telegram**: a single Unicode emoji character, e.g. `"\u{1f44d}"`
  (the actual character, not the codepoint string). Telegram restricts
  bots to a small list of recognised reactions; unknown emoji bounce
  with `BadRequest`.
- **Discord**: a Unicode emoji character or a custom-emoji reference of
  the form `"name:id"`. The adapter URL-encodes before calling the
  REST endpoint.
- **CLI / stdio**: emitted as a line `reacted: <emoji>` next to the
  original (best-effort cosmetic; the line in the transcript is not
  retroactively changed).
- Channels that do not support reactions return
  `AdapterError::Unsupported` at delivery time.

When in doubt, use the short Unicode form (`"\u{1f44d}"`). Adapters
that need a shortcode normalise both directions.

## Common reactions and their canonical forms

| Intent | Slack | Telegram (char) | Discord |
|---|---|---|---|
| thumbs up | `thumbsup` | `\u{1f44d}` | `\u{1f44d}` |
| check / done | `white_check_mark` | `\u{2705}` | `\u{2705}` |
| eyes / seen | `eyes` | `\u{1f440}` | `\u{1f440}` |
| 100 | `100` | `\u{1f4af}` | `\u{1f4af}` |

If you target multiple channels at once, write the channel-correct
form for each call.

## Adding multiple reactions

A single call adds one reaction. To add more, issue separate
`add_reaction` calls. Most platforms cap how many reactions a single
user may add per message; the adapter surfaces the cap as
`AdapterError::BadRequest` once you cross it.

## Example

Mark your own previous announcement as resolved:

```json
{ "message_id": 5, "emoji": "white_check_mark" }
```

Or on Telegram (Unicode form):

```json
{ "message_id": 5, "emoji": "\u{2705}" }
```
