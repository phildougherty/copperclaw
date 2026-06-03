-- Approval lifecycle: expiry, revocation, and an append-only decision log.
--
-- The `pending_approvals` table already carries an `expires_at` column
-- (001_initial) and a free-text `status` column. This migration does NOT
-- alter that table's shape — it widens the *meaning* of `status` to admit a
-- new terminal value `revoked` (handled in the `ApprovalStatus` enum) and
-- adds the audit surface that the lifecycle needs:
--
-- `approval_decisions` is an append-only receipt of every terminal decision
-- taken against a pending approval — approve / deny / expire / revoke. Unlike
-- `pending_approvals` (which holds at most one live row per natural key and is
-- mutated in place when a status flips), this table is never updated or
-- deleted: each decision lands a fresh row recording who acted, what they
-- decided, when, and why. It is the canonical "who approved what" record for
-- operators and post-incident review.
--
-- `decided_by` is a free-text actor label rather than a FK: decisions can be
-- taken by an operator over the admin socket ("host"), by the expiry sweep
-- ("system:expiry"), or by an agent flow — none of which map cleanly to a
-- single `users` row.
CREATE TABLE approval_decisions (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  approval_id TEXT NOT NULL REFERENCES pending_approvals(approval_id),
  action      TEXT NOT NULL,   -- the pending approval's `action` family, copied for query convenience
  outcome     TEXT NOT NULL,   -- 'approve' | 'deny' | 'expire' | 'revoke'
  decided_by  TEXT NOT NULL,   -- actor label: 'host', 'system:expiry', 'agent:<session>', ...
  reason      TEXT,            -- optional free-text rationale (e.g. revoke note)
  decided_at  TEXT NOT NULL    -- RFC3339
);

CREATE INDEX idx_approval_decisions_approval ON approval_decisions(approval_id);
CREATE INDEX idx_approval_decisions_decided_at ON approval_decisions(decided_at);
