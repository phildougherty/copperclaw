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
- `to` (optional). Accepted forms:

  | Form | Meaning |
  |---|---|
  | omitted | Default destination: your parent agent if you were spawned by one, otherwise the originating channel. |
  | `"user"` | The human in the ROOT conversation of the spawn chain. Works at any depth — a grandchild's `"user"` reaches the human, never a middle agent. |
  | `"agent:parent"` | The agent that spawned you (delivered into its inbound). Errors with a clear message if you have no parent. |
  | `"telegram:chat-9"` | A fully-qualified channel id string. Treated as `{ "kind": "channel", "id": ... }`. |
  | `{ "kind": "channel", "id": ... }` | Tagged channel form. |
  | `{ "kind": "agent", "session_id": ... }` | Another agent by session id. |
  | `{ "kind": "user", "id": "u_42" }` | A specific user by user id (host resolves the DM route). |

  Full routing semantics (named destinations, resolution order) are in
  [[destinations]].

## When to omit `to`

Omit `to` whenever you are replying to whoever sent you the inbound you
are processing. For a normal conversational session that is the human on
the originating channel; for a spawned child/grandchild agent it is the
parent that spawned you (report up, let the parent decide what reaches
the user).

Only set `to` when you are deliberately routing somewhere else:
- `"user"` — escalate to the human at the root of the spawn chain
  (e.g. a child that needs a clarification only the human can give).
  Use sparingly: the default for spawned agents is to report to the
  parent, which aggregates before anything reaches the user's chat.
- `"agent:parent"` — explicit report-up (same as the spawned-agent
  default; useful when your inbound came from a user channel but the
  reply belongs to your parent).
- A different channel the agent is wired to (e.g. report a Telegram event
  in a Slack ops room).
- Another agent by session id (delivers as a `MessageKind::Agent` row,
  bypassing channel adapters).
- A specific user across all their known DMs.

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
the same message later ([[edit-message]], [[add-reaction]]).

`send_message` is the right shape for prose. Before reaching for it
with choices, status tables, files, or errors, check the decision tree
in [[native-ui]] — those belong in `send_card`, `ask_user_question`,
or `send_file`.
