---
name: ask-user-question
description: Pose a multiple-choice question to a user with ask_user_question and read the reply back through the inbound queue.
---

# ask-user-question

`ask_user_question` is the cross-channel way to gather a constrained
answer from a human. The host renders the question with whatever UI
each channel offers (Slack buttons, Telegram inline keyboard, Discord
components, plain text fallback) and writes the user's reply back as
an ordinary inbound message you process on the next turn.

## Schema

```json
{
  "title": "Approve the deploy?",
  "options": ["yes", "no", "later"],
  "to": "slack:C01ABCD"
}
```

- `title` (required, non-blank). The text rendered above the choices.
- `options` (required, at least one non-blank entry). Plain strings;
  the user can pick exactly one.
- `to` (optional). Same forms as `send_message`. Omit to ask on the
  originating channel.

## How the reply round-trips

1. The tool ack writes a `pending_questions` row in the central DB
   carrying the question id, your session id, and the option list.
2. The host renders the question on the destination channel.
3. When the user clicks (or types) an option, the channel adapter
   emits an inbound event the host correlates back to your session
   via the pending-question id.
4. Your container's next poll loop receives the answer as a
   `MessageKind::Chat` (or `System`, depending on the adapter)
   inbound row whose content carries
   `{"question_id": "...", "answer": "yes"}`.
5. The `pending_questions` row is deleted.

Your code does **not** block on the reply. The tool returns
immediately; the user might answer in seconds, hours, or never.
Design your behaviour to be resumable.

## Timeout behaviour

There is no host-enforced timeout for the user. Pending questions sit
in the central DB indefinitely. If you need a deadline, schedule a
follow-up with `schedule_task` and check `pending_questions` for the
correlation id when it fires:

```text
ask_user_question({"title": "...", "options": [...]})
schedule_task({
  "name": "deploy-question-followup",
  "when": "<now + 15m>",
  "prompt": "If question abc-123 is still pending, fall back to safe default."
})
```

## Constraints

- At least one option must be supplied.
- Options must not be empty / whitespace-only.
- Total option count is not capped here, but channel adapters do:
  Slack tolerates ~25 buttons per actions block; Telegram fits ~8
  buttons cleanly per row. Prefer 2-5 options.
- Multiple simultaneous questions to the same user are allowed but
  confusing — try to keep at most one outstanding per thread.

## Example

```json
{
  "title": "Which environment first?",
  "options": ["staging", "prod-canary", "prod-full"]
}
```

The fallback rendering on a plain-text channel is:

```text
Which environment first?
  1) staging
  2) prod-canary
  3) prod-full
(Reply with the option text or its number.)
```
