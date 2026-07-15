# telegram / slash-clear

The telegram twin of `cli/slash-clear`, with two telegram-specific
wrinkles pinned on top of the gate bypass (group chat, engage mode
`mention`, no mention on the inbound):

- The user text is `/CLEAR@ReplayBot` — the router strips the
  `@BotName` suffix and normalises case so the RUNNER's clear-history
  sentinel (which matches `/clear` exactly) still fires, and preserves
  what the user actually typed under `content.original_text`.
- The runner handles the command synchronously: no LLM turn (no
  `claude/` turns, no `usage_report` row), one confirmation chat row,
  inbound marked completed.

Uses `runner_drain: true` because the runner's slash-command handler
does not count a turn; the harness races `run_loop` against an
inbound-drain watcher instead of `max_turns`.

Hand-authored (contract path — no live recording applicable).
