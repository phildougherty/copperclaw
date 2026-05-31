---
name: native-ui
description: Taxonomy of outbound UI shapes — when to use send_message vs send_card vs ask_user_question vs a breadcrumb vs an edit, with per-channel rendering notes. Reach for this before composing any reply that is not pure conversational prose.
---

# native-ui

Your outbound messages have a **shape**. Each channel has native
primitives — buttons, chips, embeds, edits. Pick the shape that
matches what you're doing; the host renders natively where it can.

`send_message` is the *default*, not the *only* answer. Right for
prose; wrong for choices, status reports, progress chips, approvals.
Reach for this skill before defaulting to `send_message` for anything
that isn't prose.

## Decision tree (first match wins)

1. **Invoking a slow tool** (`shell`, `web_search`, `read_file`, …).
   → Do nothing. The runner auto-emits a `MessageKind::Breadcrumb`
   chip on tool start and edits it to `Done`/`Failed` on completion
   (native chips on telegram/slack/discord/gchat/matrix). Default ON
   — operators can disable via `IRONCLAW_TOOL_BREADCRUMBS=0`. Don't
   pre-announce; that's a duplicate chip.

2. **Editing a file** (`edit_file`, `multi_edit`, `apply_patch`,
   `write_file`). → The runner auto-emits a `MessageKind::Diff` card
   alongside the breadcrumb (native diff renderers on
   telegram/slack/discord/gchat/matrix; text fallback elsewhere).
   Don't send the diff manually.

3. **Managing your todo list** (`todo_add`, `todo_update`,
   `todo_delete`). → The runner auto-emits a `MessageKind::TodoList`
   chip that edits in place on every mutation (native checkbox list
   with pin/unpin on telegram/slack/discord; native on gchat/matrix;
   text fallback elsewhere). Don't summarise the list in chat.

4. **User must pick from a small set.** → `ask_user_question`. Native
   buttons; reply round-trips via inbound queue. See
   `../ask-user-question/SKILL.md`.

5. **Operator approval before a destructive action.** → For built-in
   families (`install_packages`, `add_mcp_server`, sender, channel)
   the runner writes `pending_approvals` automatically and an
   operator clears via `iclaw approvals approve-id <id>` /
   `deny <id>` (`../approvals/SKILL.md`). For agent-defined gates use
   `send_card` with two `value` buttons: Approve (`primary`), Deny
   (`danger`). The tap routes back as inbound chat carrying the
   `value`.

6. **Structured info** — status, comparison, build summary.
   → `send_card` with `title`+`body`+`fields[]`. Native on
   telegram/slack/discord/gchat; text fallback elsewhere. See
   `../send-card/SKILL.md`.

7. **Tool failure to report.** → `send_card` titled `"Error"` with a
   body explaining what failed and what you tried. Don't use
   `send_message` — operators want scannable error chips. (The host
   ALSO auto-emits a native `MessageKind::Error` card for runtime
   failures it detects: provider timeouts, delivery retry
   exhaustion, container-unresponsive apologies — you don't need to
   surface those.)

8. **Code change.** → See #2 — the diff card fires automatically.
   Don't paste a diff in chat.

9. **Sending a file or binary.** → `send_file`. Don't base64 into a
   chat message. See `../send-file/SKILL.md`.

10. **Updating a message you already sent.** → `edit_message` with the
    saved outbound `seq`. Don't post fresh. See `../edit-message/SKILL.md`.

11. **Quick ack of someone else's message.** → `add_reaction` on the
    inbound seq, or a one-emoji `send_message`.

12. **Prose to the user.** → `send_message`.

## Per-channel rendering

How each shape lands on the five major chat channels. "native" = the
adapter renders a real platform primitive; "text" = the trait-default
text-fallback path. CLI, stdio, and webhook-only adapters render every
shape as plain text.

| Shape | telegram | slack | discord | gchat | matrix |
|---|---|---|---|---|---|
| Plain text (`send_message`) | native | native | native | native | native |
| Tool chip (`MessageKind::Breadcrumb`) | native `<code>` chip + edit-in-place | native context block + chat.update | native embed + PATCH | native cards v2 + patch | native `m.notice` + m.replace |
| Action buttons (`send_card.buttons[]` / `ask_user_question.options`) | native inline keyboard | native block-kit actions | native components action-row | native cards v2 button list | text fallback (reply with option text) |
| Card title + fields (`send_card`) | native MarkdownV2 + keyboard | native block-kit header + section | native embed + components | native cards v2 section | text fallback |
| Diff card (`MessageKind::Diff`) | native ```diff fenced + colourise | native rich_text_preformatted per hunk | native embed (diff fenced + colour) | native cards v2 monospace hunks | native `<pre><code class="language-diff">` |
| Todo list (`MessageKind::TodoList`) | native MarkdownV2 + pinChatMessage | native block-kit + pins.add | native embed + put_pin | native cards v2 decoratedText | native HTML + m.replace |
| Status badge / chip (`send_card` with one field) | native | native | native (embed colour) | native | text fallback |
| Error card (`MessageKind::Error`) | native bold + `<pre>` details | native attachments `color: danger` | native red embed | native cards v2 with severity icon | native `<font color="#cc3333">` |
| Collapsible / long output (`MessageKind::Chat` + `content.expander`) | native `<blockquote expandable>` | native overflow + "Show more" | native embed (description folding) | native cards v2 `collapsibleSection` | native `<details>` |
| Thinking block (`MessageKind::Thinking`, opt-in) | native `<blockquote expandable>` | native context-block chunked | native grey embed | native cards v2 `collapsibleSection` | native `m.notice` + `<details>` |
| Typing indicator (`TypingModule`) | native typing | native (assistants) | native typing | no (platform lacks API) | native typing |
| Reaction (`add_reaction`) | native | native | native | native (codepoint via action) | native (via action) |
| Edit (`edit_message`) | native | native | native | native (via action) | native (via action) |

Teams + Webex are landing the same surfaces via Adaptive Cards.
Composite picture across all 21 adapters lives in
`../../docs/channels/README.md`.

## Anti-patterns

Don't narrate tool calls — the breadcrumb fires for you.

```text
WRONG:  send_message("Running shell…"); shell({"command":"git status"})
RIGHT:  shell({"command":"git status"})    # breadcrumb auto-emits
```

Don't ask yes/no as prose — use `ask_user_question` with `options`.

```text
WRONG:  send_message("Should I proceed? Reply YES or NO.")
RIGHT:  ask_user_question({"question":"Proceed?","options":["Yes","No"]})
```

Don't pack fielded info into one chat line; a build summary belongs in
`send_card.fields`.

```text
WRONG:  send_message("Tests: 1247 pass, 3 fail. Duration: 42s. Coverage: 87%.")
RIGHT:  send_card({"title":"Build",
                   "fields":[{"name":"Tests","value":"1247 pass / 3 fail"},
                             {"name":"Duration","value":"42s"},
                             {"name":"Coverage","value":"87%"}]})
```

Don't post a fresh message to update status — `edit_message` the
original by its saved `seq`.

Don't put approval buttons in plain chat (`"reply CONFIRM"`); use a
`send_card` with `primary`/`danger` `value` buttons.

Don't surface raw error tracebacks via `send_message`; wrap in a card.

Don't hand-craft platform JSON (Block Kit, Discord embed) as a card
body. The schema is canonical; adapters translate.

Don't call typing yourself — the host's `TypingModule` emits it on
every tool call. See `../typing-indicator/SKILL.md`.

## In doubt

Prefer the structured primitive. Cards are forward-compatible: as
adapters add native renderers, old card calls light up automatically.

## See also

`../send-message/SKILL.md`, `../send-card/SKILL.md`,
`../send-file/SKILL.md`, `../ask-user-question/SKILL.md`,
`../edit-message/SKILL.md`, `../add-reaction/SKILL.md`,
`../typing-indicator/SKILL.md`, `../approvals/SKILL.md`,
`../error-handling/SKILL.md`, `../messaging-context/SKILL.md`.
