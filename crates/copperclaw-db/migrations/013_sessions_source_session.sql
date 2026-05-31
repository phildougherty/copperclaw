-- 013_sessions_source_session.sql
--
-- Add `source_session_id` to the `sessions` table. Populated by
-- `CreateAgentHandler` when one agent spawns another: the parent's
-- session id is stashed on the child so the runtime can route the
-- child's default `send_message` (no explicit `to:`) back to the
-- parent's `inbound.db` instead of dumping it into the user's chat.
--
-- Nullable: root sessions (no parent agent) have NULL.
--
-- Foreign-key behavior: the codebase runs `PRAGMA foreign_keys=ON` on
-- the central DB, so the `REFERENCES sessions(id) ON DELETE SET NULL`
-- clause IS enforced. Hard-DELETEing a parent session row will
-- cascade-null its children's `source_session_id` — at which point a
-- child's next `send_message(to: None)` falls back to channel routing
-- via the inherited messaging group. The intended pattern for retiring
-- a parent is `UPDATE sessions SET status='archived'`, NOT DELETE, so
-- live children keep their parent pointer.
ALTER TABLE sessions ADD COLUMN source_session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL;

-- Index for the common query "find children of session X" (used by
-- `cclaw audit list` / `cclaw health` once Phase 4 ships, and by
-- routing logic that walks the chain to find the root MG).
CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(source_session_id);
