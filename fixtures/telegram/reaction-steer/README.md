# telegram / reaction-steer

M19 U7: inbound reactions as agent-visible input.

The messaging group is a GROUP chat wired with `engage_mode = mention` and no
pattern, and the inbound reaction carries no mention. Plain text in this venue
would be mention-gated and dropped — the fixture routes ONLY because a reaction
payload (`content.reaction`) is whitelisted past the gate exactly like a button
callback (`mention::is_interaction_payload`, M19 U7).

Asserts the inbound-reaction contract lands: `kind = "chat"`, `trigger = 0`
(a reaction never spawns a container on its own — it is a lightweight steering
signal consumed by the runner's R2 mid-turn seam, not a task), the persisted
`content.reaction { emoji, target_seq, actor }`, `status = "pending"`, and that
no runner turn / outbound / delivery happens.

The runner-side leg — a 👍 on the agent's OWN last message folding an
affirmative one-line interjection within one tool-batch boundary, and an
unrelated reaction being ignored — is exercised by the runner unit tests
`mid_turn_reaction_on_own_message_folds_affirmative` and
`mid_turn_reaction_on_unrelated_message_is_ignored`
(`copperclaw-runner/src/run/drive_turn.rs`), which drive the mid-turn race the
in-process replay harness (one turn per inbound) can't stage directly. This
fixture owns the inbound → router leg, mirroring the `slash-stop` twin.

Hand-authored (contract path — no live recording applicable).
