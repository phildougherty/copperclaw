-- Host-side per-group LLM rate-limit caps.
--
-- Added as ALTER TABLE so the migration is backwards-compatible:
-- existing rows with NULL caps == no gate.
--
-- The container manager counts `agent_turns` rows in a trailing
-- 60-second and 3600-second window before every spawn.  If either
-- count meets the corresponding cap, the spawn is refused.
--
-- Both columns default to NULL (no cap), matching the OpenBSD
-- tenet of conservative defaults: operators must opt in to limits.
ALTER TABLE group_budgets ADD COLUMN agent_turns_per_minute_cap INTEGER;
ALTER TABLE group_budgets ADD COLUMN agent_turns_per_hour_cap   INTEGER;
