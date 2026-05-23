-- 013_sessions_source_session.sql
--
-- Add `source_session_id` to the `sessions` table. Populated by
-- `CreateAgentHandler` when one agent spawns another: the parent's
-- session id is stashed on the child so the runtime can route the
-- child's default `send_message` (no explicit `to:`) back to the
-- parent's `inbound.db` instead of dumping it into the user's chat.
--
-- Nullable: root sessions (no parent agent) have NULL. The column is
-- a soft reference — we don't FK-enforce across the cascade since the
-- old parent session may be archived while children continue running.
ALTER TABLE sessions ADD COLUMN source_session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL;

-- Index for the common query "find children of session X" (used by
-- `iclaw audit list` / `iclaw health` once Phase 4 ships, and by
-- routing logic that walks the chain to find the root MG).
CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(source_session_id);
