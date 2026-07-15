# telegram / slash-stop

The telegram twin of `cli/slash-stop`, with one extra assertion baked
into the seed: the messaging group is a GROUP chat wired with
`engage_mode = mention` and no pattern, and the inbound `/stop` carries
no mention. Plain text in this venue would be mention-gated and
dropped — the fixture routes ONLY because recognised slash commands
bypass the gate (M18 R1).

Asserts the control-row contract: `kind = "system"`, `trigger = 0`,
`content.control.op = "stop"`, `status = "pending"` (consumed later by
M18 R2), and that no runner turn / outbound / delivery happens.

Hand-authored (contract path — no live recording applicable).
