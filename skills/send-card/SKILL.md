---
name: send-card
description: Send a structured interactive card with the send_card MCP tool, channels that support cards, and how to write graceful fallbacks.
---

# send-card

`send_card` sends a platform-specific structured payload — a Slack
Block Kit message, a Telegram inline-keyboard reply, a Discord embed,
or anything else the destination adapter knows how to parse.

## Schema

```json
{ "to": "slack:C01ABCD", "card": { "...": "channel-specific" } }
```

- `card` (required, object). The tool itself only verifies that the
  value is a JSON object; the channel adapter validates the schema.
- `to` (optional). Same forms as `send_message` — string,
  tagged channel, tagged agent, tagged user. Omit for a reply on the
  originating channel.

The `card` payload is forwarded verbatim. Whatever you write must match
the destination adapter's expectations.

## Channels that support cards

- **Slack** (canonical use case). The adapter speaks Block Kit. Pass
  an object containing `"blocks": [ ... ]` and optional fallback
  `"text"`. Buttons, selects, multi-selects, dividers, sections, and
  images are all supported.
- **Telegram**. Renders as a message with an `inline_keyboard` reply
  markup. The card shape the adapter accepts is
  `{ "text": "...", "buttons": [[{"text":"…","callback":"…"}]] }` —
  rows of buttons, callbacks routed back to your agent as an
  `ask_user_question`-style inbound event.
- **Discord**. Renders as an embed plus action-row components. Pass
  `{ "embeds": [...], "components": [...] }` matching the Discord
  REST schema.
- **Webhook adapters**. Pass-through; the receiving system is on the
  hook to interpret.

## Channels that fall back

For platforms without a card concept (CLI, plain SMS, IRC-style
chats), the adapter renders a textual fallback. Always include a
`text` or `fallback` field in your card so the fallback is useful.

CLI rendering pattern:

```text
[card] Pick a tier:
  1. Free
  2. Pro
  3. Enterprise
```

If you cannot guarantee the channel supports cards, prefer
`ask_user_question` — it works on every channel and the host fans the
reply back to you.

## Slack example (Block Kit)

```json
{
  "to": "slack:C01ABCD",
  "card": {
    "text": "Deploy ready",
    "blocks": [
      { "type": "section",
        "text": { "type": "mrkdwn", "text": "*Deploy* ready for review" } },
      { "type": "actions", "elements": [
        { "type": "button", "text": {"type":"plain_text","text":"Approve"},
          "value": "approve", "style": "primary" },
        { "type": "button", "text": {"type":"plain_text","text":"Reject"},
          "value": "reject", "style": "danger" }
      ]}
    ]
  }
}
```

## Telegram example

```json
{
  "card": {
    "text": "Pick a tier",
    "buttons": [
      [{ "text": "Free", "callback": "tier:free" }],
      [{ "text": "Pro",  "callback": "tier:pro"  }]
    ]
  }
}
```

## Discord example

```json
{
  "card": {
    "embeds": [{ "title": "Build #42", "description": "All green." }],
    "components": [{
      "type": 1,
      "components": [
        { "type": 2, "style": 3, "label": "Tag", "custom_id": "tag" }
      ]
    }]
  }
}
```

## Result

The tool returns the outbound `seq` (use it to `edit_message` /
`add_reaction` later). The card payload is stored in
`messages_out.content`, so you can re-read it from the outbox.
