-- Per-session inbound: persist the channel-level `reply_to` and `is_group`
-- signals carried on `InboundEvent` so the runner's per-turn "Conversation
-- context" block can render them.
--
-- Both columns are nullable: every adapter populates `is_group` opportunistically
-- (Telegram knows, Slack DM threads know, CLI/file-watcher channels don't), and
-- `reply_to` is only set when the wire data carries a parent message link
-- (Telegram `message.reply_to_message`, Slack `thread_ts` reply, Discord
-- `referenced_message`, Matrix `m.in_reply_to`). Existing rows are unaffected;
-- the runner's context-block builder treats `None` the same way it does today
-- (degrade silently, don't fabricate).
--
-- Stored as TEXT: `reply_to` is the platform-side parent message id (the same
-- shape adapters already write into `platform_id`), `is_group` as a SQLite
-- INTEGER (0/1) to match the existing `trigger` / `on_wake` columns.

ALTER TABLE messages_in ADD COLUMN reply_to TEXT;
ALTER TABLE messages_in ADD COLUMN is_group INTEGER;
