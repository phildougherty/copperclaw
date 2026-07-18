//! Relevance scoring for skill selection (M22 S2).
//!
//! Ranks each skill's `description` against a task/query string so only the
//! relevant skills inline into the system prompt instead of splicing in all
//! ~41 (the all-skills prompt bloat noted in `copperclaw-runner`'s
//! `compaction.rs`). See [`crate::registry::SkillsSelector::Relevant`].
//!
//! ## Reuses the memory store's FTS path
//!
//! Rather than invent a bespoke ranking, this mirrors the full-text scoring
//! `copperclaw_db::memory` already uses for the per-group memory store
//! (migration 021 `memory_store` / `memory_fts`): an in-memory **`SQLite` FTS5**
//! index over the corpus, matched with a tokenized `MATCH` expression and
//! ranked by `SQLite`'s `bm25`, then normalized to `[0, 1]` (best match = 1.0)
//! the same way `MemoryStore::search` normalizes its FTS half. The two crates
//! deliberately don't depend on each other, so the tokenizer + normalizer are
//! reproduced here to the same contract rather than shared through a helper
//! (the memory helpers are private to `copperclaw-db`).
//!
//! Unlike the memory store there is **no** embedding/cosine half: skill
//! descriptions are short curated frontmatter with no stored vectors, so the
//! FTS5 pass alone is the ranking signal.

use rusqlite::{Connection, params};

/// One ranked skill: its name and blended relevance score in `[0, 1]`
/// (higher is better). Only skills whose `description` matched the query text
/// appear in a ranking — an off-topic query yields a short (or empty) list,
/// which is exactly how [`crate::registry::SkillRegistry::select_relevant`]
/// narrows the inlined set.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredSkill {
    pub name: String,
    pub score: f64,
}

/// Rank `(name, description)` pairs against `query`, returning only the
/// descriptions the query matched, sorted by descending score (ties broken by
/// ascending name so the order is deterministic).
///
/// Reuses the memory-store FTS5 path: an in-memory `fts5` index over the
/// descriptions, matched via [`fts_match_expr`] and ranked by `bm25`, then
/// mapped into `[0, 1]` by [`normalize_bm25`]. A query with no usable token
/// (or that matches nothing) yields an empty ranking.
///
/// # Errors
/// Propagates any [`rusqlite::Error`] from building/querying the in-memory
/// index. Callers that must not fail (skill selection) fall back to the
/// unfiltered set on error rather than surfacing it.
pub fn rank_descriptions(
    items: &[(String, String)],
    query: &str,
) -> Result<Vec<ScoredSkill>, rusqlite::Error> {
    // No usable query token -> nothing is "relevant".
    let Some(match_expr) = fts_match_expr(query) else {
        return Ok(Vec::new());
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let conn = Connection::open_in_memory()?;
    // Mirror the memory store's external-content-free FTS5 shape: one text
    // column, rowid = 1-based index into `items`.
    conn.execute_batch("CREATE VIRTUAL TABLE descr_fts USING fts5(descr);")?;
    {
        let mut insert = conn.prepare("INSERT INTO descr_fts(rowid, descr) VALUES (?1, ?2)")?;
        for (i, (_, description)) in items.iter().enumerate() {
            // rowid must be >= 1; map index i -> i + 1.
            let rowid = i64::try_from(i + 1).unwrap_or(i64::MAX);
            insert.execute(params![rowid, description])?;
        }
    }

    // Same query shape as `MemoryStore::search`: bm25 rank, lower is better.
    let mut stmt = conn.prepare(
        "SELECT rowid, bm25(descr_fts) AS rank
         FROM descr_fts
         WHERE descr_fts MATCH ?1
         ORDER BY rank",
    )?;
    let mut raw: Vec<(usize, f64)> = Vec::new();
    let rows = stmt.query_map(params![match_expr], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
    })?;
    for row in rows {
        let (rowid, rank) = row?;
        // rowid back to 0-based index.
        if let Ok(idx) = usize::try_from(rowid - 1) {
            if idx < items.len() {
                raw.push((idx, rank));
            }
        }
    }

    // Normalize bm25 (lower-is-better) to [0, 1] (higher-is-better) exactly
    // like the memory store: best raw rank -> 1.0, worst -> ~0.
    let (min_rank, max_rank) = raw
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &(_, v)| {
            (lo.min(v), hi.max(v))
        });

    let mut scored: Vec<ScoredSkill> = raw
        .into_iter()
        .map(|(idx, rank)| ScoredSkill {
            name: items[idx].0.clone(),
            score: normalize_bm25(rank, min_rank, max_rank),
        })
        .collect();

    // Descending score; ascending name on ties for a stable order.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(scored)
}

/// Map a `bm25` raw rank (lower is better) into `[0, 1]` (higher is better),
/// matching `copperclaw_db::memory::normalize_bm25`. A single hit (or an
/// all-equal set) gets the max score 1.0.
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

/// Build an FTS5 `MATCH` expression from raw query text, matching
/// `copperclaw_db::memory::fts_match_expr`: each alphanumeric token is
/// double-quoted and OR-joined into a robust "any of these terms" query, so
/// user text containing FTS5 operators / unbalanced quotes can't raise a
/// malformed-MATCH error. Returns `None` when no usable token survives.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn items(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, d)| ((*n).to_string(), (*d).to_string()))
            .collect()
    }

    #[test]
    fn ranks_on_topic_skill_first() {
        let corpus = items(&[
            (
                "git-workflow",
                "Create commits, branches, and pull requests in git",
            ),
            (
                "dataviz",
                "Build charts, graphs and dashboards to visualize data",
            ),
            (
                "deep-research",
                "Fan out web searches and synthesize a cited report",
            ),
        ]);
        let ranked = rank_descriptions(&corpus, "help me commit my branch in git").unwrap();
        assert_eq!(ranked.first().unwrap().name, "git-workflow");
        // The dataviz / research skills share no query tokens -> not returned.
        assert!(ranked.iter().all(|s| s.name != "dataviz"));
    }

    #[test]
    fn off_topic_query_returns_fewer_than_all() {
        let corpus = items(&[
            ("git-workflow", "Create commits and branches in git"),
            ("dataviz", "Build charts and dashboards to visualize data"),
            (
                "deep-research",
                "Fan out web searches and synthesize a report",
            ),
        ]);
        // A query that overlaps nothing in any description.
        let ranked = rank_descriptions(&corpus, "quantum gardening ornithology").unwrap();
        assert!(
            ranked.len() < corpus.len(),
            "off-topic query must narrow: got {} of {}",
            ranked.len(),
            corpus.len()
        );
        assert!(ranked.is_empty(), "nothing overlaps -> empty ranking");
    }

    #[test]
    fn partial_overlap_selects_only_matches() {
        let corpus = items(&[
            ("git-workflow", "Create commits and branches in git"),
            ("dataviz", "Build charts and dashboards to visualize data"),
            (
                "deep-research",
                "Fan out web searches and synthesize a report",
            ),
        ]);
        // Avoid stopwords shared across descriptions (e.g. "in"): the FTS
        // pass has no stopword removal, matching the memory store's behavior.
        let ranked = rank_descriptions(&corpus, "visualize charts dashboards").unwrap();
        let names: Vec<_> = ranked.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["dataviz"]);
        assert!(ranked[0].score > 0.0);
    }

    #[test]
    fn scores_are_normalized_and_descending() {
        let corpus = items(&[
            ("a", "deployment runbook for the telegram bot"),
            (
                "b",
                "telegram deployment notes and deployment tips deployment",
            ),
            ("c", "grocery list milk eggs bread"),
        ]);
        let ranked = rank_descriptions(&corpus, "telegram deployment").unwrap();
        assert_eq!(
            ranked.len(),
            2,
            "only the two telegram/deployment docs match"
        );
        // Best match is 1.0 after normalization; scores are non-increasing.
        assert!((ranked[0].score - 1.0).abs() < 1e-9);
        for w in ranked.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
        for s in &ranked {
            assert!((0.0..=1.0).contains(&s.score));
        }
    }

    #[test]
    fn single_match_gets_max_score() {
        let corpus = items(&[
            ("only", "kubernetes networking guide"),
            ("other", "baking sourdough bread at home"),
        ]);
        let ranked = rank_descriptions(&corpus, "kubernetes").unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].name, "only");
        assert!((ranked[0].score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_query_yields_empty_ranking() {
        let corpus = items(&[("a", "anything at all")]);
        assert!(rank_descriptions(&corpus, "   ").unwrap().is_empty());
        assert!(
            rank_descriptions(&corpus, "!!! ??? ...")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn empty_corpus_is_ok() {
        assert!(rank_descriptions(&[], "anything").unwrap().is_empty());
    }

    #[test]
    fn special_chars_in_query_do_not_error() {
        let corpus = items(&[("a", "needle in the haystack")]);
        // FTS5 operators / unbalanced quotes must be tokenized, not passed raw.
        let ranked = rank_descriptions(&corpus, "\"needle\" AND (haystack) OR *").unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].name, "a");
    }

    #[test]
    fn tie_broken_by_name_ascending() {
        // Two identical descriptions -> identical bm25 -> identical normalized
        // score (1.0); the name breaks the tie deterministically.
        let corpus = items(&[
            ("zebra", "shared keyword body"),
            ("apple", "shared keyword body"),
        ]);
        let ranked = rank_descriptions(&corpus, "keyword").unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].name, "apple");
        assert_eq!(ranked[1].name, "zebra");
    }
}
