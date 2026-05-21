## cli / text-reply

The minimal end-to-end replay fixture: one inbound `chat` line on the
local CLI channel, one Claude turn that streams back `Hello back!`, one
outbound chat row, one delivery.

This fixture is the M11 acceptance gate. It exercises:

- The router's per-wiring fanout (one `messaging_group_agents` row).
- The per-session inbound DB (one `messages_in` row, even-parity `seq`).
- The runner's tool-use loop short-circuit (no tools needed).
- The `usage_report` system-row emit at the end of every LLM turn.
- The delivery service's `session_routing` fallback for runner-emitted
  chat rows that don't carry their own destination.
- The mock channel adapter's `deliver()` recording.

The Anthropic transport is faked with a `wiremock` `MockServer` that
serves the SSE event stream in `claude/001-turn.json` verbatim.
