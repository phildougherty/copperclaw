# cli / slash-clear

Pins the M18 R1 `/clear` wiring on the cli channel: the router stamps
the row (`content.command = "clear"`, text normalised) and the RUNNER's
existing clear-history sentinel (`detect_slash_command_batch` in
`copperclaw-runner/src/run/mod.rs`) handles it synchronously — history
wiped, confirmation chat row emitted, inbound marked completed, no LLM
turn (no `claude/` turns in this fixture, no `usage_report` row).

Uses `runner_drain: true` because the runner's slash-command handler
does not count a turn; the harness races `run_loop` against an
inbound-drain watcher instead of `max_turns`.

Hand-authored (contract path — no live recording applicable).
