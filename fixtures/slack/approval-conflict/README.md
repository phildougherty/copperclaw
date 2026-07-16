# slack/approval-conflict (M19 F3 — b, "already resolved")

Approval `...e1` was already resolved (`status = 'approved'`, decision
`decided_by = 'host'` in `central.sql`) — a faster tapper or the CLI won the
race. Owner Olivia (`slack:U200`, global `Owner`) then taps **Approve** on the
still-visible card (Slack `block_actions`, callback carried under
`content.callback.value`).

The losing tap is **not** silent (the old bug): the interceptor's shared DB
resolve returns `applied = false`, and F3b posts a follow-up reply naming who
resolved it — `This request was already resolved by host.` — the only entry in
`delivered`. The card is not re-edited (the winner already stamped it), and no
inbound row is written (`messages-in` / `messages-out` empty).

Registration is explicit — see `slack_approval_conflict_already_resolved` in
`crates/copperclaw-host/tests/replay.rs`.
