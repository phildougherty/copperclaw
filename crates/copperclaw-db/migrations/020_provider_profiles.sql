-- Provider resilience: per-group fallback chains, multi-key profiles, and
-- per-channel model pinning, plus the runtime health state that drives
-- automatic degrade + recover.
--
-- M16 Phase 4 (provider resilience) layers cross-provider/model failover on
-- top of the runner's existing in-provider retry/backoff. Today a group runs
-- a single `ProviderRuntimeConfig` (one provider + one model); a rate-limit
-- (HTTP 429), an overload (529), or a provider-down (5xx) within that single
-- entry can only be retried in place — when the upstream is genuinely down
-- the group goes dark. These two tables make a group's provider posture a
-- chain rather than a point:
--
--   provider_profiles  — the *configuration*: an ordered fallback chain of
--                        (provider, model) entries for an agent group, each
--                        carrying its own multi-key rotation set, plus the
--                        optional per-channel model pin map. One row per
--                        agent group; NULL chain (no row) means "unchanged"
--                        — the group keeps its single-provider behaviour.
--
--   provider_health    — the *runtime state*: one row per (agent_group,
--                        provider, model, key_id) entry recording whether it
--                        is healthy / cooling-down after a rate-limit / down
--                        after a 5xx, with the timestamp the host may
--                        re-probe it. The host reads this on spawn to pick
--                        the highest-priority *healthy* chain entry and a
--                        healthy key, and writes it on a classified failure
--                        (degrade, audited) and on recovery (restore).
--
-- Both tables are keyed by agent_group_id and cascade-deleted with the group.
-- The default deployment (no provider_profiles row) is bit-for-bit unchanged:
-- the host's existing single-provider selection runs untouched.

CREATE TABLE IF NOT EXISTS provider_profiles (
    agent_group_id TEXT PRIMARY KEY
        REFERENCES agent_groups(id) ON DELETE CASCADE,
    -- Ordered fallback chain. JSON array of objects:
    --   [{ "provider": "anthropic", "model": "claude-sonnet-4-6",
    --      "keys": [{ "id": "primary", "api_key_env": "ANTHROPIC_API_KEY" }] },
    --    { "provider": "ollama",    "model": "qwen3.6:27b", "keys": [] }]
    -- Position 0 is the primary; the host degrades down the list and
    -- re-probes back up to position 0. An empty array / NULL means "no
    -- chain configured" — the group keeps single-provider behaviour.
    chain TEXT,
    -- Per-channel model pin. JSON object mapping channel_type -> model id:
    --   { "telegram": "claude-opus-4-1", "cli": "claude-haiku-4" }
    -- A pinned channel forces that model regardless of the active chain
    -- entry's default model (the provider is still the active entry's).
    -- NULL / empty object means "no pins".
    model_by_channel TEXT,
    -- How long (seconds) a chain entry / key stays in its degraded state
    -- before the host re-probes it. Bounds the recover interval. NULL ->
    -- the host's built-in default.
    reprobe_after_secs INTEGER,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS provider_health (
    agent_group_id TEXT NOT NULL
        REFERENCES agent_groups(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    -- Stable identifier of the key/profile within the chain entry's `keys`
    -- list. The empty string is the sentinel for "the entry has no explicit
    -- keys" (single ambient credential), so the PK is always well-formed.
    key_id TEXT NOT NULL,
    -- 'healthy' | 'rate_limited' | 'down'. Anything else reads as healthy
    -- (forward-compatible: an unknown status never wedges a group dark).
    status TEXT NOT NULL DEFAULT 'healthy',
    -- RFC3339. When set and in the future, the entry/key is cooling down and
    -- the host skips it until `now >= cooldown_until`. NULL -> immediately
    -- eligible (healthy).
    cooldown_until TEXT,
    -- RFC3339 of the most recent classified failure that degraded this row.
    last_failure_at TEXT,
    -- Short reason string for the last degrade ("rate_limit", "overloaded",
    -- "server_error", ...). Surfaced by `cclaw groups provider status`.
    last_reason TEXT,
    -- Monotonic count of consecutive failures (reset to 0 on recovery).
    failure_count INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (agent_group_id, provider, model, key_id)
);

CREATE INDEX IF NOT EXISTS idx_provider_health_group
    ON provider_health(agent_group_id);
