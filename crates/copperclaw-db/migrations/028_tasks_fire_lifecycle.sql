-- Fire-lifecycle columns for the first-class `tasks` table (A6).
--
-- The first-class scheduled-tasks table itself already exists (migration
-- 010_tasks): it carries the schedule spec, next-fire, owning group/session,
-- payload, and a four-state `status` lifecycle, and it is already the source
-- of truth for the `schedule_task` / `list_tasks` / `cancel_task` /
-- `pause_task` / `resume_task` / `update_task` tools and for the sweep's
-- due-task fan-out (`copperclaw-host-sweep::checks::scheduling`). What it
-- lacked was any durable record of a task's *firing* history — the sweep
-- bumped `next_fire` (recurring) or flipped `status` to `completed`
-- (one-shot) but never recorded that a fire happened.
--
-- This migration adds two nullable/defaulted columns the sweep now writes on
-- every fire:
--
--   last_fired_at  RFC-3339 instant of the most recent fire (NULL until the
--                  task has fired at least once).
--   fire_count     monotonically increasing count of fires (0 until first
--                  fire). A recurring task accrues one per occurrence.
--
-- Together these give operators, the metrics rider (M1's "scheduled-task
-- lifecycle" wish), and any future event-driven-trigger follow-up a durable,
-- queryable record of autonomous activity without scanning the message log.
-- Existing rows backfill to NULL / 0 via the column defaults and continue to
-- fire unchanged.

ALTER TABLE tasks ADD COLUMN last_fired_at TEXT;
ALTER TABLE tasks ADD COLUMN fire_count INTEGER NOT NULL DEFAULT 0;
