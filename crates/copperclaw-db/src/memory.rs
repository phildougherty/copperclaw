//! Per-agent-group searchable memory store (M16 Phase 3).
//!
//! Replaces the flat bind-mounted `/data/memory/` directory with a per-group
//! `SQLite` database (`memory.db`, one file per agent group) supporting
//! **hybrid retrieval**:
//!
//!   - **FTS5 full-text** — `memory_fts` is an external-content FTS5 index over
//!     `memory_entries.body`, kept in sync by triggers (see migration 021).
//!     A search query is matched against it and ranked by `SQLite`'s `bm25`.
//!   - **Vector cosine** — embeddings are stored as little-endian `f32` blobs
//!     in `memory_entries.embedding`. [`MemoryStore::search`] computes cosine
//!     similarity in pure Rust against the query embedding when one is
//!     supplied. There is deliberately **no** `sqlite-vec` / extension-loading
//!     dependency: loading a `SQLite` extension requires `unsafe` (forbidden
//!     workspace-wide) and a fragile native dep. Pure-Rust cosine over stored
//!     blobs is simpler, gate-clean, and exact for the small per-group corpora
//!     we expect.
//!
//! ## Embedding generation is deferred (honest scope)
//!
//! This store *accepts and queries* embeddings, so the vector half is real the
//! moment a caller supplies vectors. **Generating** embeddings from text is NOT
//! wired here: it must route through the host credential broker (so the
//! provider key never leaks into the container), and the broker today proxies
//! only the chat-completions path — there is no embeddings endpoint. Rather
//! than smuggle a key into the container to call an embeddings API directly
//! (the exact threat the broker exists to close), embedding generation is left
//! as a documented broker follow-up. Until then the MCP tools store text +
//! FTS5; `embedding` stays NULL and the cosine pass is a no-op.
//!
//! ## Provenance
//!
//! Every entry is tagged [`Provenance::Trusted`] or [`Provenance::Untrusted`].
//! Trusted = the agent authored it or it came from an operator; untrusted =
//! lifted verbatim from an external fetch / tool output. The runner's coarse
//! approval gate treats any turn whose context touched untrusted content as
//! tainted (see `copperclaw-runner`'s provenance gate). Taint cannot propagate
//! through the model, so the gate is necessarily coarse.

use crate::DbError;
use crate::migrate::{MigrationSet, run_migrations};
use copperclaw_types::AgentGroupId;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::path::{Path, PathBuf};

/// Provenance tag on a memory entry (and, by extension, on any context content
/// derived from it). See the module header.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// Authored by the agent or supplied by an operator — safe to act on.
    Trusted,
    /// Lifted from an external source (web fetch, third-party tool output).
    /// A turn touching this is tainted for the coarse gate.
    Untrusted,
}

impl Provenance {
    /// Stable lower-case wire form stored in the DB and passed across crates.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }

    /// Parse the wire form. Unknown / malformed values fail **safe** to
    /// [`Provenance::Untrusted`] — a row whose tag we can't read must not be
    /// treated as trusted.
    #[must_use]
    pub fn parse_or_untrusted(s: &str) -> Self {
        match s {
            "trusted" => Self::Trusted,
            _ => Self::Untrusted,
        }
    }

    /// True for [`Provenance::Untrusted`].
    #[must_use]
    pub fn is_untrusted(self) -> bool {
        matches!(self, Self::Untrusted)
    }
}

/// One stored memory entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    pub id: i64,
    pub key: String,
    pub body: String,
    pub provenance: Provenance,
    pub source: Option<String>,
    /// Decoded embedding, empty when none stored.
    pub embedding: Vec<f32>,
    pub created_at: String,
    pub updated_at: String,
}

/// One search hit: an entry plus its blended score and which signals fired.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub entry: MemoryEntry,
    /// Blended relevance score (higher is better). Combines the FTS5 rank and
    /// the cosine similarity when both are present (see [`MemoryStore::search`]).
    pub score: f64,
    /// True when the FTS5 index matched the query text.
    pub fts_matched: bool,
    /// Cosine similarity in `[-1, 1]` when a query embedding was supplied AND
    /// this row has a same-dimension embedding; `None` otherwise.
    pub cosine: Option<f32>,
}

/// A write request for [`MemoryStore::upsert`].
#[derive(Debug, Clone)]
pub struct MemoryWrite<'a> {
    pub key: &'a str,
    pub body: &'a str,
    pub provenance: Provenance,
    pub source: Option<&'a str>,
    /// Embedding to store, or empty for text-only (the common case today).
    pub embedding: &'a [f32],
}

/// Search parameters for [`MemoryStore::search`].
#[derive(Debug, Clone)]
pub struct MemoryQuery<'a> {
    /// Free-text query matched against the FTS5 index. Empty disables the
    /// full-text pass (vector-only search).
    pub text: &'a str,
    /// Query embedding for the cosine pass. Empty disables the vector pass
    /// (full-text-only search — the default today, since generation is
    /// deferred).
    pub embedding: &'a [f32],
    /// Maximum hits to return.
    pub limit: usize,
}

/// Per-agent-group memory store backed by `memory.db`.
pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// Filesystem path of a group's `memory.db` under `data_root`. Lives
    /// alongside the (now-legacy) per-group `memory/` mount root so the
    /// store is isolated per group exactly like the old mount was.
    #[must_use]
    pub fn db_path(data_root: impl AsRef<Path>, group: AgentGroupId) -> PathBuf {
        data_root
            .as_ref()
            .join("groups")
            .join(group.as_uuid().to_string())
            .join("memory.db")
    }

    /// Open (or create) the store at `path`, applying the memory migration set.
    /// The parent directory is created if absent.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )?;
        run_migrations(&mut conn, MigrationSet::Memory)?;
        Ok(Self { conn })
    }

    /// Open an in-memory store. Test-only convenience.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, DbError> {
        let mut conn = Connection::open_in_memory()?;
        run_migrations(&mut conn, MigrationSet::Memory)?;
        Ok(Self { conn })
    }

    /// Insert or replace the entry for `key`. The `updated_at` timestamp is
    /// refreshed on every write; `created_at` is preserved across overwrites.
    /// Returns the row id.
    pub fn upsert(&self, write: &MemoryWrite<'_>) -> Result<i64, DbError> {
        let now = chrono::Utc::now().to_rfc3339();
        let blob = encode_embedding(write.embedding);
        let dim = i64::try_from(write.embedding.len()).unwrap_or(0);
        // ON CONFLICT keeps the original created_at (via the table's existing
        // value) — we only touch body/provenance/source/embedding/updated_at.
        self.conn.execute(
            "INSERT INTO memory_entries
               (mem_key, body, provenance, source, embedding, embedding_dim, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(mem_key) DO UPDATE SET
               body = excluded.body,
               provenance = excluded.provenance,
               source = excluded.source,
               embedding = excluded.embedding,
               embedding_dim = excluded.embedding_dim,
               updated_at = excluded.updated_at",
            params![
                write.key,
                write.body,
                write.provenance.as_str(),
                write.source,
                blob,
                dim,
                now,
            ],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM memory_entries WHERE mem_key = ?1",
            params![write.key],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Fetch an entry by its logical key. `None` when absent.
    pub fn get(&self, key: &str) -> Result<Option<MemoryEntry>, DbError> {
        self.conn
            .query_row(
                "SELECT id, mem_key, body, provenance, source, embedding, embedding_dim,
                        created_at, updated_at
                 FROM memory_entries WHERE mem_key = ?1",
                params![key],
                row_to_entry,
            )
            .optional()
            .map_err(DbError::from)
    }

    /// Number of stored entries.
    pub fn count(&self) -> Result<i64, DbError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
            .map_err(DbError::from)
    }

    /// Hybrid search. Runs the FTS5 pass (when `query.text` is non-empty) and
    /// the cosine pass (when `query.embedding` is non-empty), unions the
    /// candidate rows, blends the scores, and returns the top `limit` hits
    /// sorted by descending score.
    ///
    /// Scoring: FTS5 contributes a normalized rank in `[0, 1]` (best match =
    /// 1.0, derived from `bm25` which is lower-is-better); cosine contributes
    /// its similarity mapped to `[0, 1]`. When both signals fire for a row the
    /// score is their average; when only one fires the score is that signal
    /// alone. Rows the FTS query matched are always candidates even with no
    /// embedding, and vice-versa.
    pub fn search(&self, query: &MemoryQuery<'_>) -> Result<Vec<MemoryHit>, DbError> {
        use std::collections::HashMap;

        let limit = query.limit.max(1);
        // id -> (fts_rank_raw, fts_matched)
        let mut fts: HashMap<i64, f64> = HashMap::new();
        let text = query.text.trim();
        if !text.is_empty() {
            if let Some(match_expr) = fts_match_expr(text) {
                let mut stmt = self.conn.prepare(
                    "SELECT m.id, bm25(memory_fts) AS rank
                     FROM memory_fts
                     JOIN memory_entries m ON m.id = memory_fts.rowid
                     WHERE memory_fts MATCH ?1
                     ORDER BY rank
                     LIMIT 200",
                )?;
                let rows = stmt.query_map(params![match_expr], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
                })?;
                for row in rows {
                    let (id, rank) = row?;
                    fts.insert(id, rank);
                }
            }
        }

        // Collect candidate rows: every FTS hit, plus (when an embedding was
        // supplied) every row with a same-dimension stored embedding.
        let query_emb = query.embedding;
        let mut candidates: HashMap<i64, MemoryEntry> = HashMap::new();
        let mut cosines: HashMap<i64, f32> = HashMap::new();

        if !query_emb.is_empty() {
            let mut stmt = self.conn.prepare(
                "SELECT id, mem_key, body, provenance, source, embedding, embedding_dim,
                        created_at, updated_at
                 FROM memory_entries WHERE embedding_dim = ?1 AND embedding IS NOT NULL",
            )?;
            let dim = i64::try_from(query_emb.len()).unwrap_or(0);
            let rows = stmt.query_map(params![dim], row_to_entry)?;
            for row in rows {
                let entry = row?;
                if let Some(c) = cosine_similarity(query_emb, &entry.embedding) {
                    cosines.insert(entry.id, c);
                    candidates.insert(entry.id, entry);
                }
            }
        }

        // Add FTS-only candidates (rows matched by text but not in the cosine set).
        let missing_fts_ids: Vec<i64> = fts
            .keys()
            .copied()
            .filter(|id| !candidates.contains_key(id))
            .collect();
        for id in missing_fts_ids {
            if let Some(entry) = self.get_by_id(id)? {
                candidates.insert(id, entry);
            }
        }

        // Normalize FTS ranks to [0, 1] (bm25 is lower-is-better and unbounded
        // below 0; map the best raw rank to 1.0 and the worst to ~0).
        let (min_rank, max_rank) = fts
            .values()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            });

        let mut hits: Vec<MemoryHit> = candidates
            .into_values()
            .map(|entry| {
                let fts_raw = fts.get(&entry.id).copied();
                let fts_matched = fts_raw.is_some();
                let fts_score = fts_raw.map(|raw| normalize_bm25(raw, min_rank, max_rank));
                let cosine = cosines.get(&entry.id).copied();
                let cosine_score = cosine.map(|c| (f64::from(c) + 1.0) / 2.0);
                let score = match (fts_score, cosine_score) {
                    (Some(f), Some(c)) => (f + c) / 2.0,
                    (Some(f), None) => f,
                    (None, Some(c)) => c,
                    (None, None) => 0.0,
                };
                MemoryHit {
                    entry,
                    score,
                    fts_matched,
                    cosine,
                }
            })
            .collect();

        // Sort by score descending, breaking ties by most-recently-updated so
        // the order is deterministic.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.entry.updated_at.cmp(&a.entry.updated_at))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    fn get_by_id(&self, id: i64) -> Result<Option<MemoryEntry>, DbError> {
        self.conn
            .query_row(
                "SELECT id, mem_key, body, provenance, source, embedding, embedding_dim,
                        created_at, updated_at
                 FROM memory_entries WHERE id = ?1",
                params![id],
                row_to_entry,
            )
            .optional()
            .map_err(DbError::from)
    }
}

/// Map a `bm25` raw rank (lower is better) into `[0, 1]` (higher is better).
/// When every candidate shares one rank (or there's a single hit) the row gets
/// the max score 1.0.
fn normalize_bm25(raw: f64, min_rank: f64, max_rank: f64) -> f64 {
    if !raw.is_finite() {
        return 0.0;
    }
    let span = max_rank - min_rank;
    if span.abs() < f64::EPSILON {
        return 1.0;
    }
    // raw == min_rank (best) -> 1.0; raw == max_rank (worst) -> 0.0.
    1.0 - ((raw - min_rank) / span)
}

/// Build an FTS5 MATCH expression from raw user text. We do NOT pass the user's
/// text directly (it could contain FTS5 operators / unbalanced quotes that
/// raise a malformed-MATCH error); instead each alphanumeric token is wrapped
/// in double quotes and OR-joined, so the query is a robust "any of these
/// terms" match. Returns `None` when no usable token survives.
fn fts_match_expr(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

/// Cosine similarity of two equal-length vectors, or `None` on a length
/// mismatch or a zero-norm vector (undefined cosine).
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

/// Encode an `f32` slice as a little-endian byte blob, or `None` (stored as
/// SQL NULL) when empty.
fn encode_embedding(v: &[f32]) -> Option<Vec<u8>> {
    if v.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    Some(out)
}

/// Decode a little-endian `f32` blob. A blob whose length isn't a multiple of
/// 4 is treated as empty (corrupt) rather than panicking.
fn decode_embedding(blob: &[u8]) -> Vec<f32> {
    if blob.len() % 4 != 0 {
        return Vec::new();
    }
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let blob: Option<Vec<u8>> = row.get("embedding")?;
    let embedding = blob.as_deref().map(decode_embedding).unwrap_or_default();
    let prov: String = row.get("provenance")?;
    Ok(MemoryEntry {
        id: row.get("id")?,
        key: row.get("mem_key")?,
        body: row.get("body")?,
        provenance: Provenance::parse_or_untrusted(&prov),
        source: row.get("source")?,
        embedding,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write<'a>(key: &'a str, body: &'a str) -> MemoryWrite<'a> {
        MemoryWrite {
            key,
            body,
            provenance: Provenance::Trusted,
            source: None,
            embedding: &[],
        }
    }

    #[test]
    fn provenance_parse_fails_safe_to_untrusted() {
        assert_eq!(
            Provenance::parse_or_untrusted("trusted"),
            Provenance::Trusted
        );
        assert_eq!(
            Provenance::parse_or_untrusted("untrusted"),
            Provenance::Untrusted
        );
        // Anything we don't recognise must NOT be treated as trusted.
        assert_eq!(
            Provenance::parse_or_untrusted("garbage"),
            Provenance::Untrusted
        );
        assert_eq!(Provenance::parse_or_untrusted(""), Provenance::Untrusted);
    }

    #[test]
    fn upsert_then_get_roundtrips() {
        let store = MemoryStore::open_in_memory().unwrap();
        let id = store.upsert(&write("k1", "the quick brown fox")).unwrap();
        assert!(id > 0);
        let got = store.get("k1").unwrap().unwrap();
        assert_eq!(got.key, "k1");
        assert_eq!(got.body, "the quick brown fox");
        assert_eq!(got.provenance, Provenance::Trusted);
        assert!(got.embedding.is_empty());
        assert!(store.get("missing").unwrap().is_none());
    }

    #[test]
    fn upsert_overwrites_body_and_provenance_for_same_key() {
        let store = MemoryStore::open_in_memory().unwrap();
        let id1 = store.upsert(&write("k", "first")).unwrap();
        let mut w = write("k", "second");
        w.provenance = Provenance::Untrusted;
        w.source = Some("web_fetch:https://x");
        let id2 = store.upsert(&w).unwrap();
        assert_eq!(id1, id2, "same key reuses the same row");
        let got = store.get("k").unwrap().unwrap();
        assert_eq!(got.body, "second");
        assert_eq!(got.provenance, Provenance::Untrusted);
        assert_eq!(got.source.as_deref(), Some("web_fetch:https://x"));
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn fts_search_finds_term_hit() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .upsert(&write("a", "deployment runbook for the telegram bot"))
            .unwrap();
        store
            .upsert(&write("b", "grocery list: milk eggs bread"))
            .unwrap();
        let hits = store
            .search(&MemoryQuery {
                text: "telegram deployment",
                embedding: &[],
                limit: 10,
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "only the runbook should match");
        assert_eq!(hits[0].entry.key, "a");
        assert!(hits[0].fts_matched);
        assert!(hits[0].cosine.is_none());
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn fts_search_handles_special_chars_without_error() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.upsert(&write("a", "needle in the haystack")).unwrap();
        // Quotes / operators that would break a raw MATCH must be tokenized.
        let hits = store
            .search(&MemoryQuery {
                text: "\"needle\" AND (haystack) OR *",
                embedding: &[],
                limit: 10,
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.key, "a");
    }

    #[test]
    fn empty_text_no_embedding_returns_nothing() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.upsert(&write("a", "anything")).unwrap();
        let hits = store
            .search(&MemoryQuery {
                text: "   ",
                embedding: &[],
                limit: 10,
            })
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn cosine_similarity_basics() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]), Some(1.0));
        let c = cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).unwrap();
        assert!(c.abs() < 1e-6, "orthogonal vectors have cosine 0");
        // Opposite direction.
        let c = cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]).unwrap();
        assert!((c + 1.0).abs() < 1e-6);
        // Length mismatch / zero norm.
        assert!(cosine_similarity(&[1.0], &[1.0, 2.0]).is_none());
        assert!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]).is_none());
    }

    #[test]
    fn embedding_blob_roundtrips() {
        let store = MemoryStore::open_in_memory().unwrap();
        let emb = vec![0.1f32, -0.5, 0.9, 0.0];
        let w = MemoryWrite {
            key: "vec",
            body: "embedded body",
            provenance: Provenance::Trusted,
            source: None,
            embedding: &emb,
        };
        store.upsert(&w).unwrap();
        let got = store.get("vec").unwrap().unwrap();
        assert_eq!(got.embedding.len(), 4);
        for (a, b) in got.embedding.iter().zip(emb.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn vector_search_ranks_nearest_embedding_first() {
        let store = MemoryStore::open_in_memory().unwrap();
        let near = vec![1.0f32, 0.0, 0.0];
        let far = vec![0.0f32, 1.0, 0.0];
        store
            .upsert(&MemoryWrite {
                key: "near",
                body: "near doc",
                provenance: Provenance::Trusted,
                source: None,
                embedding: &near,
            })
            .unwrap();
        store
            .upsert(&MemoryWrite {
                key: "far",
                body: "far doc",
                provenance: Provenance::Trusted,
                source: None,
                embedding: &far,
            })
            .unwrap();
        let q = vec![0.9f32, 0.1, 0.0];
        let hits = store
            .search(&MemoryQuery {
                text: "",
                embedding: &q,
                limit: 10,
            })
            .unwrap();
        assert_eq!(hits.len(), 2, "both rows have a same-dim embedding");
        assert_eq!(hits[0].entry.key, "near", "nearest embedding ranks first");
        assert!(hits[0].cosine.unwrap() > hits[1].cosine.unwrap());
        assert!(!hits[0].fts_matched, "vector-only search has no FTS match");
    }

    #[test]
    fn vector_search_ignores_dimension_mismatch() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .upsert(&MemoryWrite {
                key: "two",
                body: "two-dim",
                provenance: Provenance::Trusted,
                source: None,
                embedding: &[1.0, 0.0],
            })
            .unwrap();
        // Query with 3 dims must not match the 2-dim row.
        let hits = store
            .search(&MemoryQuery {
                text: "",
                embedding: &[1.0, 0.0, 0.0],
                limit: 10,
            })
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn hybrid_search_blends_fts_and_cosine() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Row A: matches text AND has a near embedding.
        store
            .upsert(&MemoryWrite {
                key: "a",
                body: "kubernetes deployment guide",
                provenance: Provenance::Trusted,
                source: None,
                embedding: &[1.0, 0.0],
            })
            .unwrap();
        // Row B: matches text only.
        store
            .upsert(&MemoryWrite {
                key: "b",
                body: "kubernetes networking notes",
                provenance: Provenance::Trusted,
                source: None,
                embedding: &[0.0, 1.0],
            })
            .unwrap();
        let hits = store
            .search(&MemoryQuery {
                text: "kubernetes",
                embedding: &[1.0, 0.0],
                limit: 10,
            })
            .unwrap();
        assert_eq!(hits.len(), 2);
        // A is both an FTS hit and the nearest vector — it should win.
        assert_eq!(hits[0].entry.key, "a");
        assert!(hits[0].fts_matched && hits[0].cosine.is_some());
    }

    #[test]
    fn search_respects_limit() {
        let store = MemoryStore::open_in_memory().unwrap();
        for i in 0..5 {
            store
                .upsert(&write(&format!("k{i}"), "shared keyword body"))
                .unwrap();
        }
        let hits = store
            .search(&MemoryQuery {
                text: "keyword",
                embedding: &[],
                limit: 2,
            })
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn untrusted_provenance_survives_roundtrip() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .upsert(&MemoryWrite {
                key: "fetched",
                body: "scraped page content",
                provenance: Provenance::Untrusted,
                source: Some("web_fetch:https://evil.example"),
                embedding: &[],
            })
            .unwrap();
        let got = store.get("fetched").unwrap().unwrap();
        assert!(got.provenance.is_untrusted());
        assert_eq!(
            got.source.as_deref(),
            Some("web_fetch:https://evil.example")
        );
    }

    #[test]
    fn db_path_is_per_group_isolated() {
        let g1 = AgentGroupId::new();
        let g2 = AgentGroupId::new();
        let p1 = MemoryStore::db_path("/data", g1);
        let p2 = MemoryStore::db_path("/data", g2);
        assert_ne!(p1, p2);
        assert!(p1.to_string_lossy().contains(&g1.as_uuid().to_string()));
        assert_eq!(p1.file_name().unwrap(), "memory.db");
    }

    #[test]
    fn open_creates_file_and_persists_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = MemoryStore::db_path(tmp.path(), AgentGroupId::new());
        {
            let store = MemoryStore::open(&path).unwrap();
            store.upsert(&write("persist", "stays on disk")).unwrap();
        }
        let store = MemoryStore::open(&path).unwrap();
        let got = store.get("persist").unwrap().unwrap();
        assert_eq!(got.body, "stays on disk");
    }
}
