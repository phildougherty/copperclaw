-- DM pairing codes.
--
-- When an unknown sender first DMs the bot, the host mints a short
-- single-use pairing code and delivers it back to that sender through the
-- ordinary outbound delivery path. The operator reads the code out of band
-- (the user relays it) and runs `cclaw pairing approve <code>` to promote
-- the sender into the central `users` table — the same trust surface the
-- sender-scope gate already consults on every inbound.
--
-- Codes are 8-character uppercase tokens, expire ~1h after minting, and are
-- rate-limited to a small number of *active* codes per channel so a flood of
-- unknown senders can't mint an unbounded queue. Consuming a code (approve)
-- flips `status` to 'consumed'; the expiry sweep flips overdue rows to
-- 'expired'. Both are terminal — only an `active`, unexpired row pairs.
CREATE TABLE IF NOT EXISTS dm_pairing_codes (
  code               TEXT PRIMARY KEY,
  channel_type       TEXT NOT NULL,
  identity           TEXT NOT NULL,
  display_name       TEXT,
  agent_group_id     TEXT,
  messaging_group_id TEXT,
  status             TEXT NOT NULL DEFAULT 'active',
  created_at         TEXT NOT NULL,
  expires_at         TEXT NOT NULL,
  consumed_at        TEXT
);

-- The rate-limit and listing queries both scan by channel + status, and the
-- expiry sweep scans by status + expires_at. One composite index serves the
-- hot lookups without a second over-specific index.
CREATE INDEX IF NOT EXISTS dm_pairing_codes_channel_status
  ON dm_pairing_codes (channel_type, status);

CREATE INDEX IF NOT EXISTS dm_pairing_codes_status_expires
  ON dm_pairing_codes (status, expires_at);
