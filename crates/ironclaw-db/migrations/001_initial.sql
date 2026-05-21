-- Initial schema for ironclaw central database.
--
-- This file captures the consolidated final state of all schema migrations.
-- Future schema changes go in separately-numbered files.

CREATE TABLE agent_groups (
  id             TEXT PRIMARY KEY,
  name           TEXT NOT NULL,
  folder         TEXT NOT NULL UNIQUE,
  agent_provider TEXT,
  created_at     TEXT NOT NULL
);

CREATE TABLE messaging_groups (
  id                    TEXT PRIMARY KEY,
  channel_type          TEXT NOT NULL,
  platform_id           TEXT NOT NULL,
  name                  TEXT,
  is_group              INTEGER NOT NULL DEFAULT 0,
  unknown_sender_policy TEXT NOT NULL DEFAULT 'strict',
  denied_at             TEXT,
  created_at            TEXT NOT NULL,
  UNIQUE(channel_type, platform_id)
);

CREATE TABLE messaging_group_agents (
  id                     TEXT PRIMARY KEY,
  messaging_group_id     TEXT NOT NULL REFERENCES messaging_groups(id),
  agent_group_id         TEXT NOT NULL REFERENCES agent_groups(id),
  engage_mode            TEXT NOT NULL,
  engage_pattern         TEXT,
  sender_scope           TEXT NOT NULL DEFAULT 'all',
  ignored_message_policy TEXT NOT NULL DEFAULT 'drop',
  session_mode           TEXT NOT NULL DEFAULT 'shared',
  priority               INTEGER NOT NULL DEFAULT 0,
  created_at             TEXT NOT NULL,
  UNIQUE(messaging_group_id, agent_group_id)
);

CREATE TABLE users (
  id           TEXT PRIMARY KEY,
  kind         TEXT NOT NULL,
  display_name TEXT,
  created_at   TEXT NOT NULL
);

CREATE TABLE user_roles (
  user_id        TEXT NOT NULL REFERENCES users(id),
  role           TEXT NOT NULL,
  agent_group_id TEXT REFERENCES agent_groups(id),
  granted_by     TEXT REFERENCES users(id),
  granted_at     TEXT NOT NULL,
  PRIMARY KEY (user_id, role, agent_group_id)
);
CREATE INDEX idx_user_roles_scope ON user_roles(agent_group_id, role);

CREATE TABLE agent_group_members (
  user_id        TEXT NOT NULL REFERENCES users(id),
  agent_group_id TEXT NOT NULL REFERENCES agent_groups(id),
  added_by       TEXT REFERENCES users(id),
  added_at       TEXT NOT NULL,
  PRIMARY KEY (user_id, agent_group_id)
);

CREATE TABLE user_dms (
  user_id            TEXT NOT NULL REFERENCES users(id),
  channel_type       TEXT NOT NULL,
  messaging_group_id TEXT NOT NULL REFERENCES messaging_groups(id),
  resolved_at        TEXT NOT NULL,
  PRIMARY KEY (user_id, channel_type)
);

CREATE TABLE sessions (
  id                 TEXT PRIMARY KEY,
  agent_group_id     TEXT NOT NULL REFERENCES agent_groups(id),
  messaging_group_id TEXT REFERENCES messaging_groups(id),
  thread_id          TEXT,
  agent_provider     TEXT,
  status             TEXT NOT NULL DEFAULT 'active',
  container_status   TEXT NOT NULL DEFAULT 'stopped',
  last_active        TEXT,
  created_at         TEXT NOT NULL
);
CREATE INDEX idx_sessions_agent_group ON sessions(agent_group_id);
CREATE INDEX idx_sessions_lookup ON sessions(messaging_group_id, thread_id);

CREATE TABLE pending_questions (
  question_id    TEXT PRIMARY KEY,
  session_id     TEXT NOT NULL REFERENCES sessions(id),
  message_out_id TEXT NOT NULL,
  platform_id    TEXT,
  channel_type   TEXT,
  thread_id      TEXT,
  title          TEXT NOT NULL,
  options_json   TEXT NOT NULL,
  created_at     TEXT NOT NULL
);

CREATE TABLE pending_approvals (
  approval_id         TEXT PRIMARY KEY,
  session_id          TEXT REFERENCES sessions(id),
  request_id          TEXT NOT NULL,
  action              TEXT NOT NULL,
  payload             TEXT NOT NULL,
  created_at          TEXT NOT NULL,
  agent_group_id      TEXT REFERENCES agent_groups(id),
  channel_type        TEXT,
  platform_id         TEXT,
  platform_message_id TEXT,
  expires_at          TEXT,
  status              TEXT NOT NULL DEFAULT 'pending',
  title               TEXT NOT NULL DEFAULT '',
  options_json        TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX idx_pending_approvals_action_status
  ON pending_approvals(action, status);

CREATE TABLE pending_channel_approvals (
  messaging_group_id TEXT PRIMARY KEY REFERENCES messaging_groups(id),
  agent_group_id     TEXT NOT NULL REFERENCES agent_groups(id),
  original_message   TEXT NOT NULL,
  approver_user_id   TEXT NOT NULL,
  created_at         TEXT NOT NULL,
  title              TEXT NOT NULL DEFAULT '',
  options_json       TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE agent_destinations (
  agent_group_id TEXT NOT NULL REFERENCES agent_groups(id),
  local_name     TEXT NOT NULL,
  target_type    TEXT NOT NULL,
  target_id      TEXT NOT NULL,
  created_at     TEXT NOT NULL,
  PRIMARY KEY (agent_group_id, local_name)
);
CREATE INDEX idx_agent_dest_target ON agent_destinations(target_type, target_id);

CREATE TABLE unregistered_senders (
  channel_type       TEXT NOT NULL,
  platform_id        TEXT NOT NULL,
  user_id            TEXT,
  sender_name        TEXT,
  reason             TEXT NOT NULL,
  messaging_group_id TEXT,
  agent_group_id     TEXT,
  message_count      INTEGER NOT NULL DEFAULT 1,
  first_seen         TEXT NOT NULL,
  last_seen          TEXT NOT NULL,
  PRIMARY KEY (channel_type, platform_id)
);
CREATE INDEX idx_unregistered_senders_last_seen
  ON unregistered_senders(last_seen);

CREATE TABLE dropped_messages (
  id                  TEXT PRIMARY KEY,
  channel_type        TEXT NOT NULL,
  platform_id         TEXT NOT NULL,
  user_id             TEXT,
  sender_name         TEXT,
  reason              TEXT NOT NULL,
  messaging_group_id  TEXT,
  agent_group_id      TEXT,
  created_at          TEXT NOT NULL
);

CREATE TABLE container_configs (
  agent_group_id          TEXT PRIMARY KEY REFERENCES agent_groups(id) ON DELETE CASCADE,
  provider                TEXT,
  model                   TEXT,
  effort                  TEXT,
  image_tag               TEXT,
  assistant_name          TEXT,
  max_messages_per_prompt INTEGER,
  skills                  TEXT NOT NULL DEFAULT '"all"',
  mcp_servers             TEXT NOT NULL DEFAULT '{}',
  packages_apt            TEXT NOT NULL DEFAULT '[]',
  packages_npm            TEXT NOT NULL DEFAULT '[]',
  additional_mounts       TEXT NOT NULL DEFAULT '[]',
  cli_scope               TEXT NOT NULL DEFAULT 'group',
  updated_at              TEXT NOT NULL
);
