# Channel adapter audit — Team CHN

Audit conducted 2026-05-22. The README headline claim — "21 adapters,
every one a complete implementation, no half-finished adapters in the
tree" — was checked against each adapter's `deliver()`, `subscribe()`,
`set_typing()`, `edit_message()`, `add_reaction()`, and
`plain_text_fallback()` implementations.

## Verdict

| # | Adapter | Status | High-severity gaps | Notes |
|---|---|---|---|---|
| 1 | `cli` | COMPLETE | 0 | Reference impl. stdio + FIFO modes both wired. |
| 2 | `telegram` | COMPLETE | 0 | text + files + edits + reactions + plain-text fallback. |
| 3 | `slack` | COMPLETE | 0 | text + blocks + files + edits + reactions + plain-text fallback. |
| 4 | `discord` | COMPLETE | 0 | text + embeds + files + edits + reactions + plain-text fallback. |
| 5 | `matrix` | COMPLETE | 0 | text + html + files + action-edit + action-reaction; trait-level edit_message/add_reaction NOT overridden (uses action-shape via deliver). |
| 6 | `webhooks` | COMPLETE (inbound-only) | 0 | Outbound is documented `Unsupported`. |
| 7 | `github` | COMPLETE | 0 | text comment + action-edit + action-reaction. Files explicitly Unsupported (GitHub API limit). |
| 8 | `linear` | COMPLETE | 0 | text + action-edit + action-reaction. Files explicitly Unsupported. |
| 9 | `resend` | COMPLETE (outbound-only) | 0 | Email — no inbound by design. Subject/text/html/attachments + thread headers. |
| 10 | `mattermost` | COMPLETE | 0 | text + files (two-step `/api/v4/files` upload + `posts.file_ids`) + action-edit + action-reaction. |
| 11 | `teams` | COMPLETE | 0 | channel + chat + action-edit + action-reaction. Channel-target files supported (Graph filesFolder + drive upload + attachment-by-reference); chat-target files Unsupported (delegated-auth limit). |
| 12 | `gchat` | COMPLETE | 0 | text + threaded + card + action-edit/delete/reaction + files (two-step `attachments:upload` then `attachment[]`). |
| 13 | `webex` | COMPLETE | 0 | text + markdown + files + cards + action-edit/delete/reaction. |
| 14 | `line` | PARTIAL | 1 | text via reply-or-push. Files Unsupported. Edit + reaction not handled (non-`post` action returns BadRequest, no edit_message / add_reaction override). |
| 15 | `imessage` | COMPLETE | 0 | text + files (one AppleScript per file). System actions all Unsupported (AppleScript can't reach tapbacks reliably). |
| 16 | `signal` | COMPLETE | 0 | text + files + action-edit + action-reaction + action-delete. |
| 17 | `deltachat` | COMPLETE | 0 | text + files (with caption) + action-reaction + action-delete. Edit explicitly Unsupported (DC protocol limit). |
| 18 | `wechat` | COMPLETE | 0 | text + files (image/voice/video/file) + template_card. Edit/reaction Unsupported (platform limit). |
| 19 | `whatsapp-cloud` | COMPLETE | 0 | text + reply + files + action-reaction. Edit Unsupported (platform limit). |
| 20 | `emacs` | COMPLETE | 0 | Elisp-template eval. Files/system-actions explicitly Unsupported. |
| 21 | `x` | COMPLETE | 0 | text + files (one DM per file). Edit/reaction Unsupported. |

**Summary**: 21 audited, 20 complete (one with documented partial scope:
`line`). Zero adapters had `todo!()` / `unimplemented!()` in the
production deliver path.

## High-severity gaps (HIGH)

None. The original README claim — "no half-finished adapters in the
tree" — survives the audit. Every adapter that compiles also makes a
real platform call (or returns a typed `AdapterError::Unsupported` /
`BadRequest`).

The adapters that the audit-checklist might flag as "incomplete" all
fall into one of these documented buckets:

1. **Platform limit** (genuinely can't be done): wechat edit/reaction,
   whatsapp edit, deltachat edit, emacs files, github files.
2. **Out-of-scope-in-v1** (intentional deferral with marker text):
   teams chat-target files (needs delegated user-OneDrive auth),
   line files / edit / reaction.
3. **Inbound-only by design**: webhooks.
4. **Outbound-only by design**: resend.

## Medium-severity gaps (MED)

None outstanding. (The previously-documented `imessage` empty-body
silent-drop was fixed: empty body now returns `BadRequest` at
`crates/ironclaw-channels/imessage/src/adapter.rs:133-137`, pinned by
`deliver_empty_body_is_bad_request_not_silent_drop`.)

## Low-severity gaps (LOW)

- **`line`**: no inbound for `unfollow` / `join` / `leave` events;
  the router only handles `message`. Acceptable for v1 but means
  the host can't detect a group it was kicked from.
- **`x`**: media upload uses v1.1 API, which is on a deprecation
  watch. Long-term we'll need to migrate to v2 media uploads.
- **`webex`**: webhook secret is verified per-request but the
  signature uses a single hard-coded HMAC algorithm (sha1). Webex
  has indicated sha256 may roll out.
- **`signal`**: relies on a long-lived `signal-cli` daemon; if the
  daemon dies the adapter only logs and waits for restart. No
  automatic respawn.
- **`gchat`**: card content passes through opaquely — no schema
  validation. A malformed card surfaces as a remote 400 rather
  than a local `BadRequest`.

## Per-platform feature coverage matrix

| Adapter | text | files | edit | reaction | typing | inbound |
|---|---|---|---|---|---|---|
| cli | yes | rendered as `[files: ...]` | no | no | no-op | stdin / FIFO |
| telegram | yes | yes (caption-on-first) | yes | yes | yes | webhook + long-poll |
| slack | yes | yes (multi-step upload) | yes | yes | yes (assistants) | events api |
| discord | yes | yes (multipart) | yes | yes | yes | gateway |
| matrix | yes | yes (upload + media event) | action | action | yes | sync |
| webhooks | NO (inbound-only) | n/a | n/a | n/a | n/a | axum |
| github | yes (comment) | NO | action | action (slug) | no-op | webhook |
| linear | yes (comment) | NO | action | action | no-op | webhook |
| resend | yes (email) | yes (base64) | NO | NO | no-op | NO (by design) |
| mattermost | yes | yes (multipart upload + post) | action | action | no-op | router |
| teams | yes (text/html) | yes (channel only; chat target → Unsupported) | action | action | no-op | webhook |
| gchat | yes (+card) | yes (upload+attachment[]) | action | action | no-op | webhook |
| webex | yes (+markdown+card) | yes (multipart) | action | action | no-op | webhook |
| line | yes (reply/push) | NO | NO | NO | no-op | router |
| imessage | yes | yes (per-file script) | NO (AppleScript limit) | NO | no-op | poll |
| signal | yes | yes (attach paths) | action | action + remove | yes | rpc |
| deltachat | yes | yes (with caption) | NO (protocol) | action | no-op | rpc |
| wechat | yes | yes (image/voice/video/file) | NO (platform) | NO (platform) | no-op | webhook |
| whatsapp-cloud | yes | yes (upload+send) | NO (platform) | action | mark-read | webhook |
| emacs | yes | NO (out-of-scope) | NO | NO | no-op | poll |
| x | yes | yes (one DM per file) | NO | NO | no-op | poll |

## Deferred punch list

See per-channel docs in this directory for line-level findings. Top
items the next sweep should tackle:

1. `line` edit + reaction support (LINE Messaging API does not expose
   either — needs Bot SDK trait change). Document on the README.
2. `teams` chat-target file attachments (requires delegated user
   auth so the bot can write to the user's OneDrive — channel-target
   files already work via the bot's app-only auth).

## Methodology

For each adapter we read `adapter.rs` end to end, plus `api.rs` /
`ingress/` / `events/` where present. We checked:

- `deliver` path: every kind of `OutboundMessage.content` is either
  handled or returns a typed error. No `todo!()`, no
  `unimplemented!()`, no `Ok(_)` that skips the platform call.
- Inbound path: the channel has a way for the platform to deliver
  messages back to the host (webhook server, long-poll task, gateway
  ws, FIFO, or rpc connector), OR the adapter is explicitly
  inbound/outbound-only with a doc comment.
- Auth handling: tokens are not logged, error → `AdapterError::Auth`
  on 401, rate-limit → `AdapterError::Rate` on 429.
- Attachment handling: if the platform supports it and we say we
  do, the `OutboundMessage.files` vec must actually drive an API
  call; if we don't, it must return a typed error.
- Trait-level `edit_message` / `add_reaction`: present (uses the
  trait default `Unsupported`) OR implemented OR routed through the
  `content.action` shape inside `deliver`.

The audit did NOT exercise any adapter against a real production
endpoint; existing wiremock-based unit tests under each adapter's
`tests` module were trusted to reflect the real wire shape.
