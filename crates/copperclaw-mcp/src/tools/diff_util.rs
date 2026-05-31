//! Shared helpers for computing a [`DiffCard`] from the pre-edit and
//! post-edit contents of a file, used by `edit_file`, `multi_edit`,
//! `apply_patch`, and `write_file` after their atomic write succeeds.
//!
//! Why this lives once rather than four times: every file-edit tool
//! ends with the same final step — *compute a structured diff from
//! `before` vs `after` strings, infer the language tag from the file
//! extension, hand the card to the [`ToolContext::emit_diff`](
//! crate::context::ToolContext::emit_diff) hook*. Centralising it here
//! means a future refinement (smarter language tags, hunk re-grouping,
//! better truncation strategy) only has to land once.
//!
//! Skip rules:
//! - File size ≥ [`BLOB_DIFF_CUTOFF_BYTES`]: emit a [`BlobReplaced`]
//!   summary card instead of a giant diff (binary blob, large media,
//!   generated artefact).
//! - `before == after`: silently skip — nothing changed.

use copperclaw_channels_core::{
    BlobReplaced, DiffCard, DiffHunk, DiffLine, DiffLineKind, BLOB_DIFF_CUTOFF_BYTES,
    MAX_DIFF_HUNKS as MAX_HUNKS,
};
use similar::{ChangeTag, TextDiff};

/// Maximum context lines to keep on each side of a change region when
/// trimming a long hunk. Tuned so a typical multi-line edit shows
/// enough context to be readable without exploding past the per-hunk
/// line cap.
const HUNK_CONTEXT_LINES: usize = 3;

/// Build a [`DiffCard`] from the pre- and post-edit string snapshots
/// of a file. Returns `None` when the snapshots are byte-identical
/// (no diff to display).
///
/// Always clamps the result before returning, so the caller can pass
/// it straight to [`crate::context::ToolContext::emit_diff`] without
/// re-validating.
pub fn build_diff_card(path: &str, before: &str, after: &str) -> Option<DiffCard> {
    if before == after {
        return None;
    }
    let language = language_for_path(path);
    let text_diff = TextDiff::from_lines(before, after);

    // Group consecutive changed regions; pull in `HUNK_CONTEXT_LINES`
    // of unchanged context on each side. `similar`'s `grouped_ops` does
    // exactly this — we just translate its op tuples into our schema.
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut added_total: u32 = 0;
    let mut removed_total: u32 = 0;
    let mut hunks_dropped = false;

    for group in text_diff.grouped_ops(HUNK_CONTEXT_LINES) {
        if group.is_empty() {
            continue;
        }
        if hunks.len() >= MAX_HUNKS {
            hunks_dropped = true;
            // Still count adds/removes in the dropped tail so the
            // footer totals stay honest.
            for op in &group {
                for change in text_diff.iter_changes(op) {
                    match change.tag() {
                        ChangeTag::Insert => added_total += 1,
                        ChangeTag::Delete => removed_total += 1,
                        ChangeTag::Equal => {}
                    }
                }
            }
            continue;
        }
        // Hunk header line numbers: `similar` ops carry the old/new
        // indices for each op; the hunk starts at the first op's
        // `old_range().start` (resp. `new_range().start`). Indices are
        // 0-based; unified diff is 1-based; convert at the boundary.
        let first_op = &group[0];
        let old_start_zero = first_op.old_range().start;
        let new_start_zero = first_op.new_range().start;

        let mut lines: Vec<DiffLine> = Vec::new();
        let mut old_len: u32 = 0;
        let mut new_len: u32 = 0;
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let value = change.value();
                // `similar` keeps the trailing `\n` on each line for
                // `from_lines`; strip it so renderers don't put a
                // blank between every line.
                let text = value
                    .strip_suffix('\n')
                    .unwrap_or(value)
                    .to_owned();
                match change.tag() {
                    ChangeTag::Equal => {
                        lines.push(DiffLine {
                            kind: DiffLineKind::Context,
                            text,
                        });
                        old_len += 1;
                        new_len += 1;
                    }
                    ChangeTag::Delete => {
                        lines.push(DiffLine {
                            kind: DiffLineKind::Remove,
                            text,
                        });
                        old_len += 1;
                        removed_total += 1;
                    }
                    ChangeTag::Insert => {
                        lines.push(DiffLine {
                            kind: DiffLineKind::Add,
                            text,
                        });
                        new_len += 1;
                        added_total += 1;
                    }
                }
            }
        }
        // Unified-diff convention is 1-based, with `,0` len for
        // empty regions. We clamp old/new len to at least 1 if the
        // op range is empty to match `diff -u`'s `@@ -0,0 +1,N @@`
        // shape for pure insertions at file start.
        //
        // File sizes that would overflow `u32` (4 G+ lines) are not a
        // shape we ship — the schema's per-hunk line cap is 60 and
        // the per-card hunk cap is 8 — but we saturate defensively
        // so a hostile/buggy snapshot never produces a wrap.
        let to_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let old_start = if old_len == 0 {
            // pure insertion: header reads `@@ -<start>,0 +… @@`
            // (0-based start), so map 0 → 0; otherwise +1 to convert.
            if old_start_zero == 0 { 0 } else { to_u32(old_start_zero) }
        } else {
            to_u32(old_start_zero + 1)
        };
        let new_start = if new_len == 0 {
            if new_start_zero == 0 { 0 } else { to_u32(new_start_zero) }
        } else {
            to_u32(new_start_zero + 1)
        };
        hunks.push(DiffHunk {
            old_start,
            old_lines: old_len,
            new_start,
            new_lines: new_len,
            lines,
        });
    }

    // Path itself gets clamped by `DiffCard::clamp` (which truncates
    // long lines / hunks); the path is passed through verbatim
    // because callers already pass display strings that fit inside
    // MAX_PATH_CHARS for any sane filesystem path. If a runaway path
    // sneaks through we let `validate` reject it.
    let mut card = DiffCard {
        path: path.to_owned(),
        language,
        hunks,
        added: added_total,
        removed: removed_total,
        truncated: hunks_dropped,
    };
    card.clamp();
    Some(card)
}

/// Build the degenerate "blob too large to diff" card when one side
/// of a write would have exceeded [`BLOB_DIFF_CUTOFF_BYTES`]. Renders
/// as a single context-line summary of the byte-size delta on every
/// adapter (no `+`/`-` clutter for a 1 MB binary blob).
pub fn build_blob_card(path: &str, before_bytes: u64, after_bytes: u64) -> DiffCard {
    BlobReplaced {
        path: path.to_owned(),
        before_bytes,
        after_bytes,
    }
    .into_card()
}

/// Decide whether the pre/post pair is small enough to diff. Either
/// side over [`BLOB_DIFF_CUTOFF_BYTES`] trips the cutoff.
pub fn over_blob_cutoff(before_bytes: u64, after_bytes: u64) -> bool {
    before_bytes >= BLOB_DIFF_CUTOFF_BYTES || after_bytes >= BLOB_DIFF_CUTOFF_BYTES
}

/// Pull a syntax-highlight language hint from the file's extension.
/// Conservative — only a handful of commonly-edited extensions; the
/// renderer falls back to a generic `diff` highlight when this returns
/// `None`. Adding more mappings is cheap; the schema cap on
/// `language` keeps the wire payload small either way.
fn language_for_path(path: &str) -> Option<String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "rb" => "ruby",
        "sh" | "bash" => "bash",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" | "hh" => "cpp",
        "lua" => "lua",
        "sql" => "sql",
        _ => return None,
    };
    Some(lang.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_diff_card_returns_none_when_unchanged() {
        assert!(build_diff_card("x.rs", "let x = 1;\n", "let x = 1;\n").is_none());
    }

    #[test]
    fn build_diff_card_captures_single_line_swap() {
        let before = "fn main() {\n    println!(\"old\");\n}\n";
        let after = "fn main() {\n    println!(\"new\");\n}\n";
        let card = build_diff_card("src/main.rs", before, after).unwrap();
        assert_eq!(card.path, "src/main.rs");
        assert_eq!(card.language.as_deref(), Some("rust"));
        assert_eq!(card.added, 1);
        assert_eq!(card.removed, 1);
        assert!(!card.truncated);
        assert_eq!(card.hunks.len(), 1);
        let lines = &card.hunks[0].lines;
        // Must include a Remove + Add for the swapped line, plus
        // surrounding context.
        let added: Vec<_> = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Add)
            .map(|l| l.text.as_str())
            .collect();
        let removed: Vec<_> = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Remove)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(added, vec!["    println!(\"new\");"]);
        assert_eq!(removed, vec!["    println!(\"old\");"]);
    }

    #[test]
    fn build_diff_card_pure_insertion_into_empty_file() {
        let card = build_diff_card("a.txt", "", "hello\nworld\n").unwrap();
        assert_eq!(card.added, 2);
        assert_eq!(card.removed, 0);
        // No `Remove` lines.
        for hunk in &card.hunks {
            for line in &hunk.lines {
                assert_ne!(line.kind, DiffLineKind::Remove);
            }
        }
    }

    #[test]
    fn build_diff_card_pure_deletion() {
        let card = build_diff_card("a.txt", "hello\nworld\n", "").unwrap();
        assert_eq!(card.added, 0);
        assert_eq!(card.removed, 2);
        for hunk in &card.hunks {
            for line in &hunk.lines {
                assert_ne!(line.kind, DiffLineKind::Add);
            }
        }
    }

    #[test]
    fn build_diff_card_truncates_at_max_hunks() {
        // Build a file with way more than MAX_HUNKS separated change
        // regions. Each region is a one-line swap surrounded by
        // unchanged context so `similar` keeps them as distinct hunks.
        let mut before = String::new();
        let mut after = String::new();
        for i in 0..(MAX_HUNKS + 5) {
            // 6 unchanged padding lines per region, then one swapped
            // line, then 6 more padding lines so adjacent regions
            // don't merge into a single hunk.
            for j in 0..6 {
                let p = format!("pad-{i}-{j}\n");
                before.push_str(&p);
                after.push_str(&p);
            }
            before.push_str(&format!("region-{i}-OLD\n"));
            after.push_str(&format!("region-{i}-NEW\n"));
            for j in 0..6 {
                let p = format!("trail-{i}-{j}\n");
                before.push_str(&p);
                after.push_str(&p);
            }
        }
        let card = build_diff_card("x.rs", &before, &after).unwrap();
        assert_eq!(card.hunks.len(), MAX_HUNKS);
        assert!(card.truncated);
        // Footer totals must still account for *every* change, even the
        // ones whose hunk was dropped.
        let expected = u32::try_from(MAX_HUNKS + 5).expect("MAX_HUNKS + 5 fits u32");
        assert!(card.added >= expected);
        assert!(card.removed >= expected);
    }

    #[test]
    fn build_blob_card_marks_truncated_and_no_real_hunks() {
        let card = build_blob_card("data/blob.bin", 1024, 4096);
        assert!(card.truncated);
        assert_eq!(card.added, 0);
        assert_eq!(card.removed, 0);
        // One synthesised context line summarising the byte delta.
        assert_eq!(card.hunks.len(), 1);
        let line = &card.hunks[0].lines[0];
        assert_eq!(line.kind, DiffLineKind::Context);
        assert!(line.text.contains("diff suppressed"));
    }

    #[test]
    fn over_blob_cutoff_trips_on_either_side() {
        assert!(!over_blob_cutoff(1024, 1024));
        assert!(over_blob_cutoff(BLOB_DIFF_CUTOFF_BYTES, 1));
        assert!(over_blob_cutoff(1, BLOB_DIFF_CUTOFF_BYTES));
    }

    #[test]
    fn language_for_path_recognises_common_extensions() {
        assert_eq!(language_for_path("foo.rs").as_deref(), Some("rust"));
        assert_eq!(language_for_path("foo.ts").as_deref(), Some("typescript"));
        assert_eq!(
            language_for_path("dir/foo.PY").as_deref(),
            Some("python"),
            "extension lookup must be case-insensitive",
        );
        assert!(language_for_path("Makefile").is_none());
    }
}
