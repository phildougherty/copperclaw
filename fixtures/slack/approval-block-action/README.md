# slack/approval-block-action (M18 G1 — in-chat approvals)

The same Owner-approves / stranger-refused flow as
`fixtures/telegram/approval-callback`, but over Slack `block_actions`, where the
tapped button's payload arrives under `content.callback.value` (Slack) rather
than `content.callback.data` (Telegram). The router's `approval_callback_data`
helper accepts either key.

1. **Owner Olivia** (`slack:U200`, global `Owner`) taps **Approve** on approval
   `...c1` → resolved via the shared DB path, card edited to `Approved by Owner
   Olivia` (addressed by the recorded `slack-ts-c1`), `ok` audit row.
2. **Stranger Sam** (`slack:U300`, registered, no role) taps **Approve** on
   `...c2` → refused with a "not authorized" reply, approval stays `pending`.

`messages-in` / `messages-out` are empty (both taps resolve router-side with
`Pending(ApprovalHandled)`); `delivered` carries only the refusal reply. DB /
audit / card-edit state is asserted in `slack_approval_block_action_round_trip`
(`crates/copperclaw-host/tests/replay.rs`).
