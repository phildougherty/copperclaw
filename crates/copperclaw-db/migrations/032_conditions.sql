-- HEARTBEAT-style condition/event check-ins (M22 A4) — durable registration for
-- the previously-dormant `copperclaw_host_sweep::checks::condition_checkin`.
--
-- Before this migration the sweep's `ConditionStore` was in-memory and
-- default-empty with no registration surface, so `IdleForAtLeastSecs` /
-- `FlagSet` conditions could never be created and never fired. A dormant
-- store that empties on restart is a weak revival; this migration makes a
-- registered condition durable — it survives a host restart and the sweep
-- reloads it into its `ConditionStore` each pass.
--
-- A condition is edge-triggered: the sweep fires a `kind:task` wake into the
-- condition's session on the RISING edge of its predicate (false-or-unseen ->
-- true), exactly as scheduled tasks / goals fan out. The rising-edge latch
-- stays in-memory (re-arming from "never seen" after a restart is the correct
-- behaviour — a still-true condition fires once more after a restart), so only
-- the condition DEFINITIONS are persisted here.
--
-- Any autonomous ACTION a condition-driven wake later takes stays gated by A2's
-- grant machinery at fire time — `grant_id` records the optional linkage; this
-- table authorizes nothing on its own.
--
-- Columns:
--   id              agent-chosen stable key (PK). Re-registering the same id
--                   REPLACES the condition (mirrors `ConditionStore::register`).
--   agent_group_id  FK -> agent_groups(id). The owning agent.
--   session_id      the session a matching predicate fans a wake into.
--   kind            'pending_inbound' | 'idle' | 'flag' — the observable tested.
--   threshold       kind-specific integer: the `min` pending count for
--                   'pending_inbound', the idle-seconds floor for 'idle'.
--                   NULL for 'flag'.
--   flag            the watched flag name for 'flag'; NULL otherwise. A 'flag'
--                   condition holds while the named row exists in
--                   `condition_flags` for its session.
--   prompt          the text delivered to the woken agent on a fire.
--   grant_id        optional FK -> task_grants(id): the A2 fire-time budget
--                   authority linkage. NULL = no linked grant.
--   created_at      row bookkeeping.
--   removed_at      soft-delete instant; NULL = active. Deregistration sets
--                   this (retaining an audit trail) rather than hard-deleting.

CREATE TABLE conditions (
  id             TEXT PRIMARY KEY,
  agent_group_id TEXT NOT NULL REFERENCES agent_groups(id),
  session_id     TEXT NOT NULL,
  kind           TEXT NOT NULL,
  threshold      INTEGER,
  flag           TEXT,
  prompt         TEXT NOT NULL,
  grant_id       TEXT REFERENCES task_grants(id),
  created_at     TEXT NOT NULL,
  removed_at     TEXT
);

CREATE INDEX idx_conditions_session ON conditions(session_id);
-- The sweep's per-pass reload scans active (not soft-removed) conditions.
CREATE INDEX idx_conditions_active ON conditions(removed_at);

-- Settable per-session latches the `flag` condition kind reads. A row's
-- PRESENCE means the flag is currently SET; clearing a flag deletes its row.
-- The sampler in the sweep reads these to populate `ConditionContext.flags_set`.
CREATE TABLE condition_flags (
  agent_group_id TEXT NOT NULL REFERENCES agent_groups(id),
  session_id     TEXT NOT NULL,
  flag           TEXT NOT NULL,
  set_at         TEXT NOT NULL,
  PRIMARY KEY (session_id, flag)
);

CREATE INDEX idx_condition_flags_session ON condition_flags(session_id);
