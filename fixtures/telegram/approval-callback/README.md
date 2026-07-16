# telegram/approval-callback (M18 G1 — in-chat approvals)

Two Telegram inline-button taps (`callback_query` → `content.callback.data`)
against pre-seeded pending approvals whose cards were "already delivered"
(`platform_message_id` set in `central.sql`):

1. **Owner Olivia** (`telegram:200`, global `Owner`) taps **Approve** on
   approval `...b1`. The router's approval interceptor (M18 G1) resolves it via
   the same DB path the CLI uses, records the decision as `decided_by = "Owner
   Olivia"`, edits the card to `Approved by Owner Olivia` (via
   `edit_message`), and writes an `ok` audit row.
2. **Stranger Sam** (`telegram:300`, registered but no role) taps **Approve**
   on approval `...b2`. Refused: a "not authorized" reply is delivered, an
   `unauthorized` audit row is written, and the approval stays `pending`.

Neither tap writes an inbound row or wakes a runner — both route outcomes are
`Pending(ApprovalHandled)`. The `messages-in` / `messages-out` streams are
therefore empty; `delivered` carries only the refusal reply. The card edit and
DB/audit state are asserted directly on the booted harness in
`telegram_approval_callback_round_trip` (`tests/replay.rs`).

Registration is explicit — see that test in `crates/copperclaw-host/tests/replay.rs`.
