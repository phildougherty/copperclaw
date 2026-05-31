-- Race regression fix for `pending_approvals::upsert`.
--
-- The original schema (`001_initial.sql`) only declared
-- `PRIMARY KEY (approval_id)` — nothing stopped two concurrent upserts
-- from the same `(request_id, action)` from both observing "no
-- existing row" and both INSERTing, producing silent duplicate
-- pending approvals. This index turns that race into a deterministic
-- update.
--
-- It is intentionally PARTIAL — `WHERE status = 'pending'` — so that
-- already-terminal rows (denied / approved / expired) remain in
-- place as historical receipts even if the same `(request_id, action)`
-- shows up again later (e.g. the user is re-asked after a previous
-- denial). The new request gets a fresh pending row.
CREATE UNIQUE INDEX IF NOT EXISTS pending_approvals_request_action_uq
  ON pending_approvals (request_id, action)
  WHERE status = 'pending';
