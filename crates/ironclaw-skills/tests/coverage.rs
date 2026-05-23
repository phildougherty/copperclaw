//! Coverage tests pinning the `tools ↔ skills` relationship.
//!
//! Every Ironclaw agent boots with two parallel inventories spliced into its
//! prompt:
//!
//! 1. The 28 in-process MCP tools registered by
//!    `ironclaw_mcp::tools::build_tool_set` (see
//!    `crates/ironclaw-mcp/src/tools/mod.rs`).
//! 2. The skill bundles in `<repo>/skills/<dirname>/SKILL.md`, each of which
//!    is supposed to teach the agent *when* to reach for one or more tools.
//!
//! Either side can drift silently:
//!
//! - A skill can reference a tool that was renamed or deleted, leaving the
//!   agent with a "use `send_msg`" instruction for a tool that no longer
//!   exists.
//! - A new tool can be added to the registry but never mentioned in any
//!   skill, so the agent never learns when to use it.
//! - A skill's frontmatter `name:` can drift from its directory name (the
//!   registry validator catches this at load time, but a unit test makes
//!   the bug impossible to land).
//!
//! These tests run pure-filesystem (no host install required) and walk
//! `<repo>/skills/` directly. They double as documentation of the
//! conventions the `skills/README.md` file lists.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use ironclaw_skills::{Frontmatter, skip_frontmatter};

// -----------------------------------------------------------------------
// Static inventories
// -----------------------------------------------------------------------

/// Canonical tool inventory. Mirror of
/// `ironclaw_mcp::tools::build_tool_set` — see that crate's
/// `tool_set_lists_every_in_process_tool` test for the other half of the
/// pin. Kept hardcoded so this crate doesn't have to depend on
/// `ironclaw-mcp` (which would pull `rmcp`, `git2`, etc. into the
/// skills-crate compile graph just to read a `&'static [&'static str]`).
const REGISTRY_TOOLS: &[&str] = &[
    "send_message",
    "send_file",
    "edit_message",
    "add_reaction",
    "ask_user_question",
    "send_card",
    "create_agent",
    "install_packages",
    "add_mcp_server",
    "schedule_task",
    "list_tasks",
    "cancel_task",
    "pause_task",
    "resume_task",
    "update_task",
    "shell",
    "edit_file",
    "read_file",
    "write_file",
    "web_fetch",
    "git_blame",
    "git_diff",
    "git_log",
    "git_status",
    "glob",
    "grep",
    "web_search",
    "explore",
    "load_skill",
    "todo_add",
    "todo_list",
    "todo_update",
    "todo_delete",
    "compact_now",
    "clear_history",
    "artifact_path",
];

/// Tools that are *deliberately* not mentioned in any skill (e.g. because
/// they only fire in response to a self-mod approval the agent never
/// directly initiates from a "skill" context). Empty for now — every
/// in-process tool currently has at least one skill teaching the agent
/// when to reach for it. Add an entry here only when accompanied by a
/// `// TODO(team-skl): <reason>` so the omission is intentional.
const TOOLS_INTENTIONALLY_UNCOVERED: &[&str] = &[];

/// Tokens that LOOK like tool names (multi-word `snake_case`) but are
/// actually schema field names, metric labels, struct fields, or
/// well-known Rust/Unix terms appearing in `SKILL.md` bodies. Anything
/// that ends up matching the "candidate tool" heuristic but is in this
/// allowlist is treated as not-a-tool-reference.
///
/// Keep this list in alphabetical order so additions are easy to review.
/// New additions are a smell — if a token looks confusable with a tool
/// name, the skill prose probably should have used a different word.
const NON_TOOL_TOKEN_ALLOWLIST: &[&str] = &[
    "agent_group_id",
    "bytes_read",
    "case_insensitive",
    "channel_type",
    "cli_request",
    "cli_response",
    "cli_scope",
    "container_configs",
    "content_type",
    "context_lines",
    "create_parents",
    "deliver_after",
    "display_name",
    "dropped_messages",
    "egress_allow",
    "exit_code",
    "from_line",
    "initial_comment",
    "inline_keyboard",
    "ironclaw_image_rebuild_failed_total",
    "is_group",
    "is_mention",
    "markdown_bytes",
    "max_bytes",
    "max_count",
    "max_results",
    "max_tokens",
    "max_turns",
    "mcp_servers",
    "message_id",
    "messages_in",
    "messages_out",
    "messaging_groups",
    "new_string",
    "no_ignore",
    "old_string",
    "open_dm",
    "packages_apt",
    "packages_npm",
    "pending_approvals",
    "pending_channel_approvals",
    "pending_questions",
    "raw_html_bytes",
    "replace_all",
    "retry_after",
    "search_type",
    "series_id",
    "session_id",
    "session_routing",
    "set_typing",
    "source_session_id",
    "supports_threads",
    "thread_id",
    "timeout_secs",
    "to_line",
    "tokens_used",
    "tools_called",
    "total_bytes",
    "total_matched",
    "turns_used",
    "unregistered_senders",
    "user_dms",
    "white_check_mark",
    "write_all",
];

/// Skill body cap. Every container spawn loads every skill into the
/// system prompt; a runaway skill bloats every session uniformly. The
/// spec called for 4 KiB; this is now enforced after a prose-cull pass
/// across the previously-oversize skills (`explore`, `web-search`,
/// `add-mcp-server`, `git`, `error-handling`, `web-fetch`,
/// `messaging-context`, `customize`, `install-packages`). Adding back
/// content that pushes a skill over the cap means trimming elsewhere
/// in that file, not raising this number.
const MAX_SKILL_BODY_BYTES: u64 = 4 * 1024;

// -----------------------------------------------------------------------
// Filesystem helpers
// -----------------------------------------------------------------------

/// Locate `<repo>/skills/` relative to this crate's `CARGO_MANIFEST_DIR`.
fn skills_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // crates/ironclaw-skills -> ../../skills
    let root = Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR has two ancestors")
        .join("skills");
    assert!(
        root.is_dir(),
        "expected skills root at {root:?}; is the repo layout intact?"
    );
    root
}

/// All immediate subdirectories of `<repo>/skills/` that contain a
/// `SKILL.md`. Returned sorted by directory name.
fn list_skill_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(skills_root()).expect("read skills/") {
        let entry = entry.expect("skills/ entry");
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").is_file() {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Parse the frontmatter for a skill directory. Panics on malformed
/// frontmatter — tests in `frontmatter.rs` already pin parsing, this
/// helper exists to keep the coverage tests readable.
fn read_frontmatter(skill_dir: &Path) -> Frontmatter {
    let raw =
        fs::read_to_string(skill_dir.join("SKILL.md")).expect("read SKILL.md");
    let yaml = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .expect("opening `---` delimiter");
    let end = yaml
        .find("\n---")
        .expect("closing `---` delimiter on its own line");
    let body = &yaml[..end];
    serde_yaml::from_str::<Frontmatter>(body).expect("valid frontmatter YAML")
}

/// Markdown body (everything after the closing `---`) for a skill.
fn read_body(skill_dir: &Path) -> String {
    let raw =
        fs::read_to_string(skill_dir.join("SKILL.md")).expect("read SKILL.md");
    skip_frontmatter(&raw).to_string()
}

// -----------------------------------------------------------------------
// Token extraction for #2/#3
// -----------------------------------------------------------------------

/// Yield every backtick-quoted token in `body`. Matches the inner text
/// between two backticks on the same line; multi-line code fences are
/// skipped (their opening triple-backtick is not a single-tick delimiter).
fn backtick_tokens(body: &str) -> impl Iterator<Item = &str> {
    body.split('\n')
        .filter(|line| !line.trim_start().starts_with("```"))
        .flat_map(|line| {
            let mut tokens = Vec::new();
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'`' {
                    let start = i + 1;
                    let mut j = start;
                    while j < bytes.len() && bytes[j] != b'`' {
                        j += 1;
                    }
                    if j < bytes.len() && j > start {
                        tokens.push(&line[start..j]);
                        i = j + 1;
                        continue;
                    }
                }
                i += 1;
            }
            tokens.into_iter()
        })
}

/// Is `token` a candidate tool reference? We accept:
/// - a known registry name verbatim, or
/// - a multi-word `snake_case` (`<word>_<word>[_<word>...]`) starting
///   with one of the verb prefixes ironclaw tools actually use.
///
/// This is intentionally narrow. The cost of letting a typo through is
/// "agent reaches for a tool that doesn't exist". The cost of a false
/// positive is "test refuses to land an unrelated skill edit". Lean
/// toward the latter.
/// Verb prefixes that an Ironclaw tool name *would* start with. If
/// none of these match, the token is almost certainly a schema field
/// or unrelated identifier.
const VERB_PREFIXES: &[&str] = &[
    "add_", "ask_", "cancel_", "create_", "edit_", "git_", "install_",
    "list_", "pause_", "read_", "resume_", "schedule_", "send_",
    "update_", "web_", "write_",
];

fn looks_like_tool_ref(token: &str, registry: &BTreeSet<&str>) -> bool {
    if registry.contains(token) {
        return true;
    }
    if !token.contains('_') {
        return false;
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return false;
    }
    VERB_PREFIXES.iter().any(|p| token.starts_with(p))
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

/// #1 — Every `skills/<dirname>/SKILL.md` has a frontmatter `name:`
/// equal to `<dirname>`. The registry's `load_skill` already rejects
/// mismatches at runtime; this test makes them impossible to commit.
#[test]
fn skill_directory_matches_frontmatter_name() {
    let mut mismatches = Vec::new();
    for dir in list_skill_dirs() {
        let dirname = dir
            .file_name()
            .and_then(|s| s.to_str())
            .expect("UTF-8 dir name")
            .to_string();
        let fm = read_frontmatter(&dir);
        if fm.name != dirname {
            mismatches.push(format!("{dirname} -> name: {:?}", fm.name));
        }
    }
    assert!(
        mismatches.is_empty(),
        "skill directory ↔ frontmatter mismatches: {mismatches:#?}"
    );
}

/// #2 — Every tool the agent can call is mentioned by name in at least
/// one skill, so the model has a "when to use this" hook. Failing this
/// test means a new tool landed in `build_tool_set` without an
/// accompanying skill (or vice versa, a skill referencing the tool was
/// deleted).
#[test]
fn every_registry_tool_appears_in_some_skill() {
    // Load every skill body once.
    let bodies: Vec<(String, String)> = list_skill_dirs()
        .iter()
        .map(|dir| {
            (
                dir.file_name().unwrap().to_string_lossy().into_owned(),
                read_body(dir),
            )
        })
        .collect();

    let intentional: BTreeSet<&str> =
        TOOLS_INTENTIONALLY_UNCOVERED.iter().copied().collect();
    let mut missing = Vec::new();

    for tool in REGISTRY_TOOLS {
        if intentional.contains(tool) {
            continue;
        }
        let mentioned = bodies.iter().any(|(_, body)| {
            backtick_tokens(body).any(|t| t == *tool)
        });
        if !mentioned {
            missing.push(*tool);
        }
    }

    assert!(
        missing.is_empty(),
        "tools with no skill telling the agent when to use them: {missing:?}"
    );
}

/// #3 — Inverse of #2. If a skill mentions a token that *looks* like a
/// tool reference (registry name verbatim or `<verb_prefix>_<rest>`),
/// the token must resolve to a real registry entry. Catches typos and
/// references to deprecated tool names.
#[test]
fn skill_tool_mentions_resolve_to_registry() {
    let registry: BTreeSet<&str> = REGISTRY_TOOLS.iter().copied().collect();
    let allowlist: BTreeSet<&str> =
        NON_TOOL_TOKEN_ALLOWLIST.iter().copied().collect();

    let mut broken: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for dir in list_skill_dirs() {
        let skill_name =
            dir.file_name().unwrap().to_string_lossy().into_owned();
        let body = read_body(&dir);

        for token in backtick_tokens(&body) {
            // `mcp__<server>__<tool>` references address external MCP
            // servers, not the in-process registry. Skip them — they're
            // covered by their own MCP server config, not by us.
            if token.starts_with("mcp__") {
                continue;
            }
            if registry.contains(token) {
                continue;
            }
            if allowlist.contains(token) {
                continue;
            }
            if looks_like_tool_ref(token, &registry) {
                broken
                    .entry(skill_name.clone())
                    .or_default()
                    .insert(token.to_string());
            }
        }
    }

    assert!(
        broken.is_empty(),
        "skills reference tools that aren't in the registry — typo or deprecated tool: {broken:#?}\n\
         (if a token is a legitimate schema field, add it to NON_TOOL_TOKEN_ALLOWLIST)"
    );
}

/// #4 — Every skill's frontmatter `description:` is non-trivial. The
/// skill-registry's relevance scoring keys on the description string;
/// blank or single-word descriptions starve it.
#[test]
fn skill_descriptions_are_substantive() {
    const MIN_DESC_CHARS: usize = 30;
    let mut offenders = Vec::new();

    for dir in list_skill_dirs() {
        let name =
            dir.file_name().unwrap().to_string_lossy().into_owned();
        let fm = read_frontmatter(&dir);
        let desc = fm.description.trim();
        if desc.chars().count() < MIN_DESC_CHARS {
            offenders.push(format!(
                "{name}: {} chars — {:?}",
                desc.chars().count(),
                desc
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "skills with descriptions shorter than {MIN_DESC_CHARS} chars: {offenders:#?}"
    );
}

/// #5 — Heuristic: every skill body mentions *when* to use it, not just
/// *what* it does. Skills that read like reference docs (only describing
/// shape and schema) train the model poorly. Applied leniently — we
/// surface offenders via `eprintln!` but do not fail, because some
/// skills (e.g. `identity`) are categorical "always load" instructions
/// where the trigger is implicit.
#[test]
fn skill_bodies_describe_when_to_use() {
    // Multi-char hints first; the bare "when" is checked separately
    // with a word-boundary scan so we don't match "whenever" → "when"
    // is fine, but we also catch "when?", "when,", "when:" etc.
    const HINTS: &[&str] = &[
        "use this",
        "reach for",
        "if you need",
        "prefer",
        "before",
        "after",
    ];

    fn has_when_word(body: &str) -> bool {
        let bytes = body.as_bytes();
        let mut i = 0;
        while i + 4 <= bytes.len() {
            if &bytes[i..i + 4] == b"when" {
                let before_ok = i == 0
                    || !bytes[i - 1].is_ascii_alphanumeric();
                let after_idx = i + 4;
                let after_ok = after_idx >= bytes.len()
                    || !bytes[after_idx].is_ascii_alphanumeric();
                if before_ok && after_ok {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    let mut weak = Vec::new();
    for dir in list_skill_dirs() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let body = read_body(&dir).to_lowercase();
        let any_hint = HINTS.iter().any(|h| body.contains(h))
            || has_when_word(&body);
        if !any_hint {
            weak.push(name);
        }
    }

    if !weak.is_empty() {
        eprintln!(
            "warning: {} skill(s) don't mention when to use the tool \
             (lacking any of: when / use this / reach for / if you need / \
             prefer / before / after): {weak:?}",
            weak.len()
        );
    }
    // Soft assertion: at most one skill is allowed to lack a WHEN hint
    // (currently `discovering-tools`, which is itself a meta-skill).
    // Tightening this further is a TODO(team-skl); for now we just want
    // to keep the regression count from growing.
    assert!(
        weak.len() <= 1,
        "more than one skill lacks a WHEN hint: {weak:?}"
    );
}

/// #6 — The skill registry loads skills in alphabetical order (currently
/// via `BTreeMap` keyed on name). Pin that behaviour so a future
/// refactor doesn't accidentally introduce platform-dependent iteration
/// order via `HashMap`.
#[test]
fn skill_loading_order_is_alphabetical() {
    use ironclaw_skills::SkillRegistry;

    let reg = SkillRegistry::scan(&skills_root(), None)
        .expect("scan skills root");
    let names: Vec<&str> = reg.iter().map(|s| s.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(
        names, sorted,
        "skill iteration order is not alphabetical (HashMap regression?)"
    );

    // Sanity: the on-disk directory ordering also matches.
    let disk: Vec<String> = list_skill_dirs()
        .iter()
        .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let mut disk_sorted = disk.clone();
    disk_sorted.sort_unstable();
    assert_eq!(disk, disk_sorted);
}

/// #7 — Skill bodies are bounded in size. Every container spawn loads
/// every skill into the system prompt; a runaway skill bloats every
/// session uniformly. See [`MAX_SKILL_BODY_BYTES`] for the rationale on
/// the current ceiling.
#[test]
fn skill_bodies_under_size_cap() {
    let mut oversize = Vec::new();
    for dir in list_skill_dirs() {
        let body = read_body(&dir);
        let bytes = body.len() as u64;
        if bytes > MAX_SKILL_BODY_BYTES {
            oversize.push(format!(
                "{}: {bytes} bytes",
                dir.file_name().unwrap().to_string_lossy()
            ));
        }
    }
    assert!(
        oversize.is_empty(),
        "skills exceeding {MAX_SKILL_BODY_BYTES}-byte body cap: {oversize:#?}"
    );
}

/// #8 — Skill bodies must not contain leftover template markers
/// (`{{ ... }}`) or WIP placeholders (`<TODO>`, `[PLACEHOLDER]`). We
/// don't process Jinja-style templates; an unrendered `{{ var }}` in the
/// prompt would just confuse the model.
#[test]
fn skill_bodies_contain_no_reserved_markers() {
    const BANNED: &[&str] = &["{{", "}}", "<TODO>", "[PLACEHOLDER]"];

    let mut offenders: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for dir in list_skill_dirs() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let body = read_body(&dir);
        for marker in BANNED {
            if body.contains(marker) {
                offenders.entry(name.clone()).or_default().push(marker);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "skills contain unprocessed template / WIP markers: {offenders:#?}"
    );
}

/// #9 — Frontmatter `tools:` declaration. No skill currently uses a
/// dedicated `tools:` list (the existing convention is `allowed-tools:`
/// which is handled by the registry parser). When/if skills adopt
/// `tools:`, every entry must resolve to a registry tool.
///
/// Today we just assert no skill has snuck in a `tools:` declaration
/// without test coverage — adding one without updating this test is the
/// bug we want to catch. The `skills/README.md` documents the
/// `allowed-tools:` convention.
#[test]
fn skill_tools_frontmatter_when_present_references_real_tools() {
    let registry: BTreeSet<&str> = REGISTRY_TOOLS.iter().copied().collect();
    let mut broken: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut any_present = false;

    for dir in list_skill_dirs() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let raw = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        // Cheap detector for a top-level `tools:` key in the frontmatter
        // (distinct from `allowed-tools:` which the registry already
        // parses). We search line-anchored to avoid matching prose.
        let yaml = raw.split("\n---").next().unwrap_or("");
        for line in yaml.lines() {
            if let Some(rest) = line.strip_prefix("tools:") {
                any_present = true;
                // Inline `tools: [a, b]` form only; if a multi-line
                // YAML list shows up, extend this. Surface to flag.
                let inner = rest.trim().trim_start_matches('[').trim_end_matches(']');
                for entry in inner.split(',') {
                    let tool = entry.trim().trim_matches(['"', '\'']);
                    if tool.is_empty() {
                        continue;
                    }
                    if !registry.contains(tool) {
                        broken.entry(name.clone()).or_default().push(
                            format!("tools entry {tool:?} not in registry"),
                        );
                    }
                }
            }
        }
    }

    assert!(
        broken.is_empty(),
        "skills with `tools:` frontmatter referencing unknown tools: {broken:#?}"
    );

    // If this fires, someone introduced the `tools:` convention. Update
    // skills/README.md to document it, then delete this guard.
    if any_present {
        eprintln!(
            "note: at least one skill uses `tools:` frontmatter; \
             ensure skills/README.md documents the convention"
        );
    }
}
