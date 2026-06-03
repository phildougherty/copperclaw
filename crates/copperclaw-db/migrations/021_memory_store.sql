-- Per-group searchable memory store (M16 security-hardening Phase 3).
--
-- Replaces the flat bind-mounted `/data/memory/` directory with a per-group
-- SQLite database supporting hybrid retrieval: FTS5 full-text search plus a
-- pure-Rust cosine-similarity pass over stored embedding blobs. There is no
-- sqlite-vec / extension-loading dependency — extension loading requires
-- `unsafe` (forbidden workspace-wide) and a fragile native dep; the vector
-- half is computed in Rust over `f32` blobs stored here. See
-- `crates/copperclaw-db/src/memory.rs`.
--
-- This migration set is applied to a DEDICATED per-group `memory.db` (one file
-- per agent group), NOT the central DB and NOT a per-session DB — keeping the
-- store isolated per group exactly like the old per-group memory mount was.
--
-- PROVENANCE: every entry carries a `provenance` tag — 'trusted' or
-- 'untrusted'. Content the agent authored or that came from an operator is
-- trusted; content lifted verbatim from an external fetch / tool output (e.g.
-- a `web_fetch` body) is untrusted. The runner's coarse approval gate treats a
-- turn whose context contains ANY untrusted-provenance content as tainted and
-- blocks credentialed external actions absent fresh approval. Taint does not
-- propagate through the model, so the gate is necessarily coarse (documented).

CREATE TABLE memory_entries (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Caller-supplied logical key (e.g. "project/preferences"). Unique per
  -- store: writing the same key overwrites the prior entry's body in the
  -- application layer (this migration just enforces uniqueness).
  mem_key     TEXT NOT NULL UNIQUE,
  -- The stored text. Indexed for FTS5 via the external-content table below.
  body        TEXT NOT NULL,
  -- 'trusted' | 'untrusted' — see the module header.
  provenance  TEXT NOT NULL DEFAULT 'trusted',
  -- Optional source label (e.g. "web_fetch:https://...", "agent", "operator").
  -- Free text, for operator/debug context; not used by the gate.
  source      TEXT,
  -- Optional embedding vector, little-endian f32 blob (4 bytes per dim).
  -- NULL when no embedding was supplied (the FTS5 half still works). The
  -- cosine pass skips rows with a NULL or dimension-mismatched embedding.
  embedding   BLOB,
  -- Number of f32 dimensions in `embedding` (0 when absent). Stored so the
  -- cosine pass can reject dimension mismatches without decoding the blob.
  embedding_dim INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL,    -- RFC3339
  updated_at  TEXT NOT NULL     -- RFC3339
);

CREATE INDEX idx_memory_entries_provenance ON memory_entries(provenance);
CREATE INDEX idx_memory_entries_updated_at ON memory_entries(updated_at);

-- FTS5 full-text index over the body, external-content-linked to
-- `memory_entries` so we don't duplicate the body text. `content_rowid` maps
-- the FTS rowid to `memory_entries.id`. Triggers below keep the index in sync.
CREATE VIRTUAL TABLE memory_fts USING fts5(
  body,
  content='memory_entries',
  content_rowid='id'
);

CREATE TRIGGER memory_entries_ai AFTER INSERT ON memory_entries BEGIN
  INSERT INTO memory_fts(rowid, body) VALUES (new.id, new.body);
END;

CREATE TRIGGER memory_entries_ad AFTER DELETE ON memory_entries BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;

CREATE TRIGGER memory_entries_au AFTER UPDATE ON memory_entries BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, body) VALUES ('delete', old.id, old.body);
  INSERT INTO memory_fts(rowid, body) VALUES (new.id, new.body);
END;
