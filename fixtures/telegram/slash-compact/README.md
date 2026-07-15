# telegram / slash-compact

The telegram twin of `cli/slash-compact`, in a mention-gated GROUP chat
(engage mode `mention`, no mention on the inbound): the fixture routes
only because recognised slash commands bypass the gate (M18 R1).

The RUNNER's compaction sentinel handles the command synchronously;
with a fresh history the pass is a no-op (`0 entries -> 0`) and no
provider call is made — the fixture has no `claude/` turns, so any
regression that starts waking the LLM for `/compact` fails loudly.

Uses `runner_drain: true` because the runner's slash-command handler
does not count a turn; the harness races `run_loop` against an
inbound-drain watcher instead of `max_turns`.

Hand-authored (contract path — no live recording applicable).
