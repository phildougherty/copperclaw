-- First-class long-running goals (M22 A3) — a durable objective the sweep can
-- drive against. Autonomy before this migration is only a scheduled prompt
-- string (migration 010/028 `tasks`) + the in-session `agent_todos.json` +
-- the per-group memory store; a multi-day objective had no durable
-- status/progress/budget anywhere. A `goals` row is that missing index.
--
-- Decision (d) — a goal INDEXES OVER the existing stores, it does NOT replace
-- them. `agent_todos.json` remains the in-session plan; the memory store
-- remains the fact store. The goal records the durable objective, its status,
-- an append-only progress log (the `goal_progress` child table), and its
-- cumulative token spend — and links to the driving task and/or the A1 grant
-- rather than re-implementing a budget authority (see `grant_id` below).
--
-- How the sweep drives it: an active goal with `next_checkin <= now` is fired
-- by `copperclaw_host_sweep::checks::goals`, which synthesises a `kind:task`
-- inbound into the goal's session (the SAME fan-out scheduled tasks use — no
-- parallel wake mechanism) so the agent wakes, reports progress via
-- `update_goal`, and the goal's `next_checkin` is re-armed from
-- `checkin_recurrence`.
--
-- Budget (decision (d)): where `grant_id` is set, the live A1 grant
-- (`task_grants.effective_grant`) is the budget AUTHORITY — the sweep gates
-- further check-ins on it, so an exhausted grant pauses the goal. `token_budget`
-- is the goal's OWN cap, consulted only when no grant is linked.
-- `tokens_consumed` accrues cumulatively for reporting either way.
--
-- Columns:
--   id                  goal uuid (PK).
--   agent_group_id      FK → agent_groups(id). The owning agent.
--   session_id          the session a check-in wake fans into (mirrors
--                       `tasks.session_id`; the goal drives one session).
--   objective           the durable objective text (what the goal is FOR).
--   status              'active' | 'paused' | 'completed' | 'abandoned'.
--                       `active` is the only state the sweep fires; `completed`
--                       and `abandoned` are terminal.
--   task_id             optional FK → tasks(id): the recurring check-in task
--                       backing this goal, when one drives it. NULL for a goal
--                       the sweep check-ins directly via `checkin_recurrence`.
--   grant_id            optional FK → task_grants(id): the budget authority
--                       (decision (d)). NULL = the goal is budgeted by its own
--                       `token_budget` (or unbounded).
--   token_budget        the goal's own cumulative token cap; consulted ONLY
--                       when `grant_id` is NULL. NULL = no own cap.
--   tokens_consumed     running cumulative spend recorded against the goal for
--                       reporting (accrued by `update_goal` progress entries).
--   checkin_recurrence  optional cron (croner) the sweep re-arms `next_checkin`
--                       from after each fire. NULL = no automatic re-arm.
--   checkin_prompt      optional prompt injected on a check-in wake; NULL =
--                       the sweep synthesises one from `objective`.
--   next_checkin        absolute RFC-3339 instant of the next due check-in;
--                       NULL = no pending check-in (paused, terminal, or a goal
--                       driven only by an external `task_id`).
--   last_checkin_at     instant of the most recent check-in fire; NULL until
--                       the first.
--   checkin_count       monotonically increasing count of check-in fires.
--   created_at/updated_at  row bookkeeping.

CREATE TABLE goals (
  id                 TEXT PRIMARY KEY,
  agent_group_id     TEXT NOT NULL REFERENCES agent_groups(id),
  session_id         TEXT NOT NULL,
  objective          TEXT NOT NULL,
  status             TEXT NOT NULL DEFAULT 'active',
  task_id            TEXT REFERENCES tasks(id),
  grant_id           TEXT REFERENCES task_grants(id),
  token_budget       INTEGER,
  tokens_consumed    INTEGER NOT NULL DEFAULT 0,
  checkin_recurrence TEXT,
  checkin_prompt     TEXT,
  next_checkin       TEXT,
  last_checkin_at    TEXT,
  checkin_count      INTEGER NOT NULL DEFAULT 0,
  created_at         TEXT NOT NULL,
  updated_at         TEXT NOT NULL
);

CREATE INDEX idx_goals_session ON goals(session_id);
CREATE INDEX idx_goals_group ON goals(agent_group_id);
-- The sweep's due-check-in scan filters `status = 'active' AND next_checkin <= now`.
CREATE INDEX idx_goals_due ON goals(status, next_checkin);

-- Append-only progress log for a goal (decision (d): the goal's durable
-- progress record, distinct from the in-session todo plan). Each `update_goal`
-- progress report appends one row here and accrues its `tokens` (when given)
-- onto `goals.tokens_consumed`. Rows are never mutated or deleted.
CREATE TABLE goal_progress (
  id          TEXT PRIMARY KEY,
  goal_id     TEXT NOT NULL REFERENCES goals(id),
  note        TEXT NOT NULL,
  tokens      INTEGER,
  created_at  TEXT NOT NULL
);

CREATE INDEX idx_goal_progress_goal ON goal_progress(goal_id, created_at);
