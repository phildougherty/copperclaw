# cli / slash-stop

Pins the M18 R1 `/stop` control-row contract end to end on the cli
channel:

- The router parses `/stop` and persists a CONTROL row into the
  session's `messages_in`: `kind = "system"`, `trigger = 0`,
  `content.control.op = "stop"`, `content.command = "stop"`,
  `content.text = "/stop"`.
- The row stays `status = "pending"` — the M18 R2 card (mid-turn
  interruption) is its consumer; R1's job ends at persistence.
- `trigger = 0` means the container manager's spawn classifier
  (`messages_in::count_due`) ignores the row, so no runner turn fires:
  `messages_out` and the delivered stream both stay empty. The harness
  mirrors that gate.

Hand-authored (error/contract path — no live recording applicable).
