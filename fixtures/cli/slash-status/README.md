# cli / slash-status

Pins the M18 R1 `/status` host-answer path on the cli channel:

- The router parses `/status` and answers it FROM HOST STATE: session
  row + agent-group name from the central DB plus a `count_due` against
  the session's `inbound.db`.
- No `messages_in` row is written (`messages-in.jsonl` is empty) and no
  runner turn fires — the reply is synthesized by the router and
  written straight to `messages_out` with explicit channel routing.
- The delivery loop then hands the reply to the channel adapter like
  any runner-emitted row (one `delivered` entry).

Hand-authored (host-answer contract path — no live recording
applicable).
