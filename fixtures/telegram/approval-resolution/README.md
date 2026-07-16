# telegram/approval-resolution (M19 F3 — a + c)

One Telegram inline-button tap exercising two approval-card correctness fixes:

1. **Fallback-id path (F3a).** Approval `...d1`'s card was delivered with **no**
   `platform_message_id` (the delivering adapter reported no editable anchor;
   `platform_message_id IS NULL` in `central.sql`). Owner Olivia
   (`telegram:200`, global `Owner`) taps **Approve**. The interceptor resolves
   it via the shared DB path, but because there is no card to edit it posts the
   resolution as a follow-up **reply** (`Approved by Owner Olivia`) instead of
   silently leaving live Approve/Deny buttons. That reply is the only entry in
   `delivered`.
2. **Silent-expiry stamp (F3c).** Approval `...d3` lapsed its TTL
   (`expires_at` in the past) while its card was still live. The tap's
   opportunistic expiry sweep (`handlers::approvals::expire_and_edit_cards`,
   run at the top of the interceptor) flips `...d3` to `expired` and edits its
   card (`tg-exp-card`) to the terminal "expired" text. That edit is asserted
   directly on the harness in `telegram_approval_resolution_fallback_and_expiry`
   (`tests/replay.rs`).

Neither tap writes an inbound row or wakes a runner — the route outcome is
`Pending(ApprovalHandled)`, so `messages-in` / `messages-out` are empty.

Registration is explicit — see that test in `crates/copperclaw-host/tests/replay.rs`.
