-- Migration 007: per-group container config extensions.
--
-- Adds three new columns to `container_configs`:
--
-- * `config_fingerprint` (TEXT) — sha256 of the rebuild-relevant fields
--   (`packages_apt`, `packages_npm`, `mcp_servers`, `skills`).  The
--   container manager compares this against the image that is currently
--   tagged in `image_tag`; a mismatch triggers a rebuild before the next
--   spawn.  NULL means "not yet fingerprinted" which the manager treats
--   conservatively as a fingerprint mismatch.
--
-- * `egress_allow` (TEXT, JSON array of "host:port" strings) — opt-in
--   network lockdown list.  Empty array (the default) means allow-all.
--   When non-empty the container manager translates each entry into an
--   iptables DROP + ACCEPT rule pair on the container's network
--   namespace (Docker runtime) or returns Unsupported (Apple runtime).
--
-- * `resource_limits` (TEXT, JSON object) — optional per-group resource
--   caps forwarded to the container runtime at spawn.  Supported keys:
--   "cpus" (string, e.g. "1.5"), "memory_mb" (integer), "pids_limit"
--   (integer).  Absent keys mean "no cap".  Docker runtime maps these
--   to --cpus / --memory / --pids-limit.  Apple runtime returns
--   Unsupported when any key is set.
--
-- All three columns default to safe/permissive values so existing rows
-- continue to work without migration-time data surgery.

ALTER TABLE container_configs
  ADD COLUMN config_fingerprint  TEXT;

ALTER TABLE container_configs
  ADD COLUMN egress_allow         TEXT NOT NULL DEFAULT '[]';

ALTER TABLE container_configs
  ADD COLUMN resource_limits      TEXT NOT NULL DEFAULT '{}';
