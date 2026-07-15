# cli / slash-compact

Pins the M18 R1 `/compact` wiring on the cli channel: the router stamps
the row (`content.command = "compact"`) and the RUNNER's existing
compaction sentinel handles it synchronously. With a fresh (empty)
history the compaction pass is a no-op (`0 entries -> 0`) and no
provider call is made — the fixture has no `claude/` turns, so any
regression that starts waking the LLM for `/compact` fails loudly.

Uses `runner_drain: true` because the runner's slash-command handler
does not count a turn; the harness races `run_loop` against an
inbound-drain watcher instead of `max_turns`.

Hand-authored (contract path — no live recording applicable).
