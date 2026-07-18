-- Task capability grants (M22 A1) — a child of the first-class `tasks` table
-- (migrations 010 + 028 lineage). A grant is the durable, human-approved,
-- bounded authorization that lets an AUTONOMOUS (scheduled / heartbeat) fire of
-- a task take a real external action instead of only drafting one.
--
-- Why a separate table (not columns on `tasks`): a task is a schedule; a grant
-- is an authorization with its own lifecycle (approve → consume → revoke /
-- expire) and its own bounds (scope + token budget + max fires + expiry). One
-- task has at most one *live* grant at a time but may accumulate a history of
-- superseded / revoked ones, so a 1-to-many child keyed on `task_id` is the
-- right shape.
--
-- SECURE-BY-DEFAULT — the whole point of this table is the autonomy brake in
-- reverse, so the schema is written so a MISSING or NON-LIVE grant reads inert:
--
--   * A row exists ONLY after a human approved it. The pending state lives in
--     `pending_approvals` (raised at schedule time, reusing the `save_skill`
--     approval round-trip); the approval APPLY arm is what INSERTs the row
--     here, already `status = 'approved'`. There is no way to land an
--     unapproved grant in this table through the normal path — so "no row" and
--     "not approved" are the same observable state.
--   * Grants are BOUNDED, never blanket: `capability_scope` names explicit
--     capability classes, and `token_budget` / `max_fires` / `expires_at`
--     cap what the grant can spend before it goes inert. Decision (b): no
--     standing / always-approved grants.
--   * Grants are REVOCABLE (`revoked_at` + `status = 'revoked'`) and EXPIRING
--     (`expires_at` absolute instant). The read API (`effective_grant`)
--     collapses approved-AND-not-revoked-AND-not-expired-AND-budget-remaining
--     into a single Option so consumers (A2's autonomy gate) never re-derive
--     liveness.
--
-- Columns:
--   id                grant uuid (PK).
--   task_id           FK → tasks(id). The task this grant authorizes.
--   capability_scope  space-separated set of scope tokens. Each token is
--                     `class` or `class:resource` (e.g. `send_message:telegram`).
--                     A bare `class` (or `class:*`) is a class-level grant that
--                     covers every resource in that class; `class:resource`
--                     matches only that exact resource. Matching semantics are
--                     documented on `copperclaw_db::tables::task_grants` — A2
--                     matches an action's required capability against this set.
--   token_budget      max tokens this grant may spend across all its fires;
--                     NULL = no token bound (still bounded by fires + expiry).
--   tokens_consumed   running total spent; grant reads inert once it reaches
--                     `token_budget`.
--   max_fires         max autonomous fires this grant authorizes; NULL = no
--                     fire bound (still bounded by budget + expiry).
--   fires_consumed    running count of authorized fires; grant reads inert once
--                     it reaches `max_fires`.
--   expires_at        absolute RFC-3339 instant after which the grant is inert.
--                     NULL is accepted at the schema level but the authoring
--                     tool requires it (no non-expiring grants, decision (b)).
--   granted_by        operator identity that approved the grant (audit).
--   status            'approved' | 'revoked'. Only ever inserted 'approved'
--                     (by the approval apply arm); `revoke` flips it.
--   approved_at       instant the human approved (stamped at insert).
--   revoked_at        instant of revocation; NULL while live.
--   created_at/updated_at  row bookkeeping.

CREATE TABLE task_grants (
  id                TEXT PRIMARY KEY,
  task_id           TEXT NOT NULL REFERENCES tasks(id),
  capability_scope  TEXT NOT NULL,
  token_budget      INTEGER,
  tokens_consumed   INTEGER NOT NULL DEFAULT 0,
  max_fires         INTEGER,
  fires_consumed    INTEGER NOT NULL DEFAULT 0,
  expires_at        TEXT,
  granted_by        TEXT,
  status            TEXT NOT NULL DEFAULT 'approved',
  approved_at       TEXT,
  revoked_at        TEXT,
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL
);

CREATE INDEX idx_task_grants_task ON task_grants(task_id);
CREATE INDEX idx_task_grants_live ON task_grants(task_id, status);
