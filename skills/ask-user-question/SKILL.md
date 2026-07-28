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

The normal call — no `to`, which asks the user you're already talking to:

```json
{
  "title": "Approve the deploy?",
  "options": ["yes", "no", "later"]
}
```

- `title` (required, non-blank). The text rendered above the choices.
- `options` (required, at least one non-blank entry). Plain strings;
  the user can pick exactly one.
- `to` (optional — **omit it** unless you're redirecting the question to
  a different channel/user than the one you're talking to). When you do
  set it, use the *same forms as `send_message`*: a fully-qualified
  channel-id **string** (`"slack:C01ABCD"`), or an object with an explicit
  `kind` — `{"kind":"user","id":"..."}`, `{"kind":"channel","id":"..."}`,
  or `{"kind":"agent","session_id":"..."}`. A bare object without `kind`
  (e.g. `{"user":"..."}`) is rejected — that's the #1 mistake. When in
  doubt, leave `to` out.

## How the reply round-trips

1. The host records a `pending_questions` row (question id, your
   session id, title, options) and renders the question on the
   destination channel — native buttons where the platform has them.
2. When the user taps an option (or just types), the reply lands as an
   **ordinary inbound chat message** on a later turn. A button tap
   arrives as a chat row whose text is the option's value; free text
   arrives as-is. There is no special `answer` payload — read it like
   any other message.

Your code does **not** block on the reply. The tool returns
immediately; the user might answer in seconds, hours, or never.
Design your behaviour to be resumable.

## Timeout behaviour

Unanswered questions expire after a default TTL of 24 hours. The
host's sweep then stamps the original card with an expiry note (no
live buttons left behind) and writes a system inbound row carrying a
question-result payload with `status = "expired"`, which
you will see as a `[system]` line on your next turn — treat it as
"no answer" and proceed with a safe default. If you need a shorter
deadline, schedule a follow-up with `schedule_task`:

```text
ask_user_question({"title": "...", "options": [...]})
schedule_task({
  "name": "deploy-question-followup",
  "when": "<now + 15m>",
  "prompt": "If the deploy question is still unanswered, fall back to safe default."
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

Use `ask_user_question` for a constrained pick; for richer structured
output (fields, mixed buttons, links) use `send_card` — see
[[send-card]] and [[native-ui]].
