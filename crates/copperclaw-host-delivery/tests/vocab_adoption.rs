//! Vocab adoption gate (M22 A4).
//!
//! `copperclaw_channels_core::vocab` is the single source of truth for
//! the transcript glyphs (rail markers, todo checkboxes, layout
//! strings). Unlike `capabilities.rs` — a mirror guarded against drift
//! by `edit_capable_edit_message_drift.rs` — vocab needs the opposite
//! guard: no adapter may *bypass* it by hardcoding a glyph literal in
//! rendering code. This test is that gate. Like the drift guard, it
//! lives in `host-delivery` because this is the one crate positioned to
//! read every sibling adapter source tree.
//!
//! # What is scanned, and why this way
//!
//! The walk covers `crates/copperclaw-channels/*/src/**/*.rs`. Per
//! file, the scan is line-based with three pragmatic rules, chosen as
//! the simplest approach that is correct for the current tree (each
//! carve-out below is a deliberate tradeoff, not an oversight):
//!
//! 1. **Test code is exempt.** Scanning stops at the first line
//!    containing `#[cfg(test)]` — the repo convention puts the test
//!    module at the bottom of each file. Test literals are a *feature*:
//!    the M22 byte-identity discipline requires tests to assert
//!    rendered output against hardcoded literals (asserting against
//!    vocab constants would be circular). Tradeoff: a rendering site
//!    added *below* a test module would escape the gate; the tree has
//!    no such layout and rustfmt'd convention keeps it that way.
//!
//! 2. **Comment lines are exempt.** Lines whose trimmed text starts
//!    with `//` (incl. `///` / `//!`) are skipped — doc comments quote
//!    example output (that is what they are for). Tradeoff: a glyph in
//!    a *trailing* comment on a code line is still scanned; today no
//!    such line exists, and a false positive there fails loudly (easy
//!    to fix) rather than silently passing.
//!
//! 3. **ASCII checkbox literals are matched only inside string
//!    literals** — extracted per line by a small quote-walker that
//!    honours `\"` escapes — because `[x]` / `[ ]` as raw source text
//!    would false-positive on ordinary indexing (`arr[x]`) and slice
//!    patterns. The RAIL codepoints, by contrast, are matched anywhere
//!    on a code line (they cannot appear in identifiers, and a char
//!    literal like `'\u{00D7}'` must be caught too), and their
//!    `\u{XXXX}` escape spellings are matched as well so an escaped
//!    hardcoding cannot slip past the raw-char check.
//!
//! # Allowlist
//!
//! - `core/src/vocab.rs` — the source of truth itself.
//! - `core/src/reaction.rs` — reaction emoji are PROTOCOL VALUES
//!   (inbound steering signals / outbound `setMessageReaction`
//!   payloads), never presentation; vocab must not absorb them.
//! - any `*/src/config.rs` — per-channel config defaults document
//!   protocol values (reaction emoji defaults), same rationale.

use std::path::{Path, PathBuf};

/// `crates/copperclaw-channels`, resolved from this crate's manifest
/// dir (`crates/copperclaw-host-delivery`), mirroring the edit-capable
/// drift guard.
fn channels_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("host-delivery manifest dir has a parent (`crates/`)")
        .join("copperclaw-channels")
}

/// The RAIL glyph codepoints, denied as raw characters in code.
const RAIL_CODEPOINTS: [char; 6] = [
    '\u{23FA}', // BLACK CIRCLE FOR RECORD — step bullet / done
    '\u{23BF}', // BOTTOM LEFT CROP — result leader
    '\u{25B0}', // BLACK PARALLELOGRAM — progress filled
    '\u{25B1}', // WHITE PARALLELOGRAM — progress empty
    '\u{25CB}', // WHITE CIRCLE — active
    '\u{00D7}', // MULTIPLICATION SIGN — failed
];

/// The same codepoints as `\u{...}` escape spellings (upper- and
/// lowercase hex), denied anywhere on a code line so a hardcoded escape
/// cannot dodge the raw-char check.
const RAIL_ESCAPES: [&str; 12] = [
    "\\u{23FA}",
    "\\u{23fa}",
    "\\u{23BF}",
    "\\u{23bf}",
    "\\u{25B0}",
    "\\u{25b0}",
    "\\u{25B1}",
    "\\u{25b1}",
    "\\u{25CB}",
    "\\u{25cb}",
    "\\u{00D7}",
    "\\u{00d7}",
];

/// The ASCII glyph literals, denied inside string literals in code.
const ASCII_GLYPHS: [&str; 5] = ["[x]", "[~]", "[!]", "[ ]", "[ok]"];

/// Files exempt from the gate, as `/`-separated paths relative to
/// `crates/copperclaw-channels`.
fn is_allowlisted(rel: &str) -> bool {
    rel == "core/src/vocab.rs" || rel == "core/src/reaction.rs" || rel.ends_with("/src/config.rs")
}

/// Contents of the double-quoted string literals on one source line.
/// Honours `\"` and `\\` escapes; an unterminated quote yields the rest
/// of the line (conservative — scanned rather than skipped).
fn string_literal_contents(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars();
    'outer: while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut content = String::new();
        loop {
            match chars.next() {
                None => break, // unterminated on this line — keep what we have
                Some('"') => {
                    out.push(content);
                    continue 'outer;
                }
                Some('\\') => {
                    // Keep the escape verbatim so `\u{23FA}` spellings
                    // remain visible to the caller's substring checks.
                    content.push('\\');
                    if let Some(next) = chars.next() {
                        content.push(next);
                    }
                }
                Some(other) => content.push(other),
            }
        }
        out.push(content);
        break;
    }
    out
}

/// Recursively collect every `.rs` file under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("readable dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// One denied literal found in non-test, non-comment adapter code.
struct Offense {
    rel_path: String,
    line_no: usize,
    matched: String,
}

fn scan_file(rel_path: &str, body: &str) -> Vec<Offense> {
    let mut offenses = Vec::new();
    for (idx, line) in body.lines().enumerate() {
        // Rule 1: the repo convention puts the test module last; test
        // literals below this point are the byte-identity discipline.
        if line.contains("#[cfg(test)]") {
            break;
        }
        // Rule 2: comment lines quote example output on purpose.
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut push = |matched: &str| {
            offenses.push(Offense {
                rel_path: rel_path.to_string(),
                line_no: idx + 1,
                matched: matched.to_string(),
            });
        };
        // RAIL codepoints: raw chars anywhere on the code line...
        for cp in RAIL_CODEPOINTS {
            if line.contains(cp) {
                push(&format!("{cp:?} (U+{:04X})", cp as u32));
            }
        }
        // ...and their `\u{...}` escape spellings.
        for esc in RAIL_ESCAPES {
            if line.contains(esc) {
                push(esc);
            }
        }
        // Rule 3: ASCII checkbox literals only inside string literals.
        for content in string_literal_contents(line) {
            for glyph in ASCII_GLYPHS {
                if content.contains(glyph) {
                    push(&format!("{glyph:?}"));
                }
            }
        }
    }
    offenses
}

#[test]
fn adapter_sources_route_glyphs_through_vocab() {
    let base = channels_dir();
    assert!(
        base.is_dir(),
        "channels dir not found at {} — if the crate moved, update this gate",
        base.display(),
    );
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&base).expect("readable channels dir") {
        let crate_dir = entry.expect("readable dir entry").path();
        let src = crate_dir.join("src");
        if src.is_dir() {
            collect_rs_files(&src, &mut files);
        }
    }
    // Sanity: a broken walk must never green-light the gate. The tree
    // has 21 adapter crates + core; well over 40 source files.
    assert!(
        files.len() >= 40,
        "vocab adoption gate walked only {} files under {} — the walk is \
         broken, not the tree clean",
        files.len(),
        base.display(),
    );

    let mut offenses = Vec::new();
    let mut saw_allowlisted = false;
    for path in &files {
        let rel = path
            .strip_prefix(&base)
            .expect("walked file is under channels dir")
            .to_string_lossy()
            .replace('\\', "/");
        if is_allowlisted(&rel) {
            saw_allowlisted = true;
            continue;
        }
        let body = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        offenses.extend(scan_file(&rel, &body));
    }
    // Sanity: the allowlist must keep matching real paths — if vocab.rs
    // moves, this gate must be updated rather than silently scanning a
    // stale allowlist.
    assert!(
        saw_allowlisted,
        "no allowlisted file (core/src/vocab.rs, ...) was seen — the \
         allowlist paths are stale; update this gate",
    );

    if !offenses.is_empty() {
        let mut msg = String::from(
            "hardcoded transcript glyphs found in adapter rendering code. \
             Glyphs are owned by `copperclaw_channels_core::vocab` — route \
             each site through `vocab::for_channel(CHANNEL_TYPE_STR)` \
             (`.rail.for_status(..)` / `.todo.get(..)` / `.layout`) and pin \
             the output with a byte-identity test (hardcoded expected \
             literals belong in #[cfg(test)], which this gate exempts). \
             Offenders:\n",
        );
        for o in &offenses {
            msg.push_str(&format!(
                "  crates/copperclaw-channels/{}:{} contains {}\n",
                o.rel_path, o.line_no, o.matched
            ));
        }
        panic!("{msg}");
    }
}

#[test]
fn string_literal_extractor_handles_escapes_and_indexing() {
    // `arr[x]` outside a string must not be seen by the ASCII check.
    assert_eq!(
        string_literal_contents(r"let y = arr[x];"),
        Vec::<String>::new()
    );
    // Plain literal.
    assert_eq!(
        string_literal_contents(r#"push("[x] done")"#),
        vec!["[x] done".to_string()]
    );
    // Escaped quote inside a literal does not terminate it.
    assert_eq!(
        string_literal_contents(r#"a("say \"[~]\" now")"#),
        vec!["say \\\"[~]\\\" now".to_string()]
    );
    // Two literals on one line are both captured.
    assert_eq!(
        string_literal_contents(r#"f("[ ]", "[ok]")"#),
        vec!["[ ]".to_string(), "[ok]".to_string()]
    );
    // Escape spellings survive verbatim for the RAIL_ESCAPES check.
    assert_eq!(
        string_literal_contents(r#"s("\u{23FA}")"#),
        vec!["\\u{23FA}".to_string()]
    );
}

#[test]
fn scan_flags_code_but_exempts_comments_and_tests() {
    let src = "\
fn render() -> &'static str {
    // doc example: [x] done and \u{23FA} bullet — exempt
    \"[x]\"
}
#[cfg(test)]
mod tests {
    const T: &str = \"[ok] \\u{25CB}\"; // exempt: byte-identity test literal
}
";
    let offenses = scan_file("fake/src/render.rs", src);
    assert_eq!(offenses.len(), 1, "exactly the code-line literal");
    assert_eq!(offenses[0].line_no, 3);
    assert!(offenses[0].matched.contains("[x]"));
}
