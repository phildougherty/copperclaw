//! Agent-authored persistent skills (M19 A4).
//!
//! Closes the `write_file` → discovery loop: an agent that has worked out a
//! good repeatable procedure can durably save it as a reusable skill. The
//! save is **approval-gated** at the host (see the `save_skill` MCP tool and
//! the host's `save_skill` approval action) — this module is the pure
//! validate-and-write core the approved side calls.
//!
//! A saved skill lands in the group's per-group override directory
//! (`<groups_dir>/<agent_group_id>/skills/<name>/SKILL.md`), which
//! [`crate::registry::SkillRegistry::scan`] already scans at the next
//! container spawn — so a saved skill is discovered and exposed to the agent
//! on the next session with no extra wiring. This is a **capability, not a
//! registry**: skills are per-group only, never shared across groups.
//!
//! ## Validation (reuses the discovery-time rules)
//! The proposed `SKILL.md` is held to the exact rules
//! [`crate::registry::load_skill`] enforces at discovery, so a saved skill can
//! never be one that discovery would later reject:
//! - the YAML frontmatter must parse and carry `name` + `description`
//!   ([`crate::frontmatter::parse`]);
//! - the `name` must be kebab-case ([`crate::name::validate`]);
//! - the frontmatter `name` must equal the requested skill name (which
//!   becomes the on-disk directory name — the `name == dir` invariant).
//!
//! ## Containment
//! The write destination is canonicalized and checked to lie under a
//! configured allowed root, reusing the same [`is_under_any`] guard
//! [`crate::materialize`] uses (lib.rs:26-29). Combined with the kebab-case
//! name rule (no `/`, `.`, or `..` can appear in a valid name) a saved skill
//! can never escape the group's skills directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::SkillError;
use crate::frontmatter::{self, Frontmatter};
use crate::materialize::is_under_any;
use crate::name;

/// Validate a proposed `SKILL.md` against the discovery-time rules WITHOUT
/// writing anything. Pure — no filesystem access — so the `save_skill` tool
/// can refuse an invalid skill synchronously (with the precise error) before
/// any approval is raised, and [`save_group_skill`] can share the exact same
/// checks it applies before writing.
///
/// The rules mirror [`crate::registry::load_skill`] so a skill that validates
/// here can never be one discovery would later reject:
/// - the frontmatter parses and carries `name` + `description`;
/// - `name` is kebab-case;
/// - the frontmatter `name` equals the requested skill name (the on-disk
///   directory slug — the `name == dir` invariant).
///
/// Returns the parsed [`Frontmatter`] on success.
///
/// # Errors
/// - [`SkillError::Frontmatter`] for a missing/malformed frontmatter (carries
///   the precise parse error).
/// - [`SkillError::InvalidName`] if `name` is not kebab-case or the
///   frontmatter `name` does not match `name`.
pub fn validate_skill_content(name: &str, content: &str) -> Result<Frontmatter, SkillError> {
    let fm = frontmatter::parse(content)?;
    name::validate(name)?;
    if fm.name != name {
        return Err(SkillError::InvalidName(format!(
            "frontmatter name {:?} does not match requested skill name {name:?}",
            fm.name
        )));
    }
    Ok(fm)
}

/// Validate a proposed `SKILL.md` and write it into a group's per-group
/// skills override directory.
///
/// - `group_skills_dir` is the destination parent
///   (`<groups_dir>/<agent_group_id>/skills`). Created if missing.
/// - `allowed_root` bounds where the write may land (typically the host's
///   `groups_dir`). When non-empty roots are supplied the canonical
///   destination must fall under one of them or the write is refused with
///   [`SkillError::EscapedRoot`]. Pass an empty slice to disable the check
///   (tests using `tempfile` paths).
/// - `name` is the requested skill name (the on-disk directory slug); it must
///   be kebab-case and equal the frontmatter `name`.
/// - `content` is the full `SKILL.md` text including its YAML frontmatter.
///
/// On success the skill directory (`<group_skills_dir>/<name>`) is created,
/// `SKILL.md` is written (overwriting any prior body for the same name — a
/// re-save updates the skill), and the skill directory path is returned.
///
/// ## Versioning (M22 S3)
/// The skill's frontmatter carries a monotonic `version` (default 1). This
/// function is **version-aware**: a *first* save keeps the content's declared
/// version (or 1 when the field is absent), while a *re-save* (a `SKILL.md`
/// already exists for this name) reads the on-disk version and writes
/// `on_disk + 1`, so re-saving durably bumps the version rather than blindly
/// overwriting. The version is always (re)written into the persisted
/// frontmatter regardless of what the caller supplied — the on-disk skill is
/// the source of truth, and [`list_group_skills`] reports it.
///
/// # Errors
/// - [`SkillError::Frontmatter`] if the frontmatter is missing/malformed or
///   omits `name`/`description` — carries the precise parse error.
/// - [`SkillError::InvalidName`] if `name` is not kebab-case or the
///   frontmatter `name` does not match `name`.
/// - [`SkillError::EscapedRoot`] if the canonical destination escapes
///   `allowed_root`.
/// - [`SkillError::Io`] on any filesystem failure.
pub fn save_group_skill(
    group_skills_dir: &Path,
    allowed_root: &[PathBuf],
    name: &str,
    content: &str,
) -> Result<PathBuf, SkillError> {
    // 1-3. Validate the frontmatter + name against the discovery-time rules
    //    (parse, kebab-case, `name == dir`). Surfaces the precise error.
    let fm = validate_skill_content(name, content)?;

    // 4. Ensure the destination parent exists so it can be canonicalized, then
    //    enforce containment against the allowed roots (reusing the same guard
    //    `materialize` applies to discovered skills).
    fs::create_dir_all(group_skills_dir).map_err(|e| SkillError::io(group_skills_dir, e))?;
    if !allowed_root.is_empty() {
        let canonical_roots: Vec<PathBuf> = allowed_root
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();
        let canonical_dest = group_skills_dir
            .canonicalize()
            .map_err(|e| SkillError::io(group_skills_dir, e))?;
        if !is_under_any(&canonical_dest, &canonical_roots) {
            return Err(SkillError::EscapedRoot {
                target: canonical_dest,
            });
        }
    }

    // 5. Write the skill. `name` is kebab-case (checked above), so it cannot
    //    contain a path separator or `..` — the join stays inside the parent.
    let skill_dir = group_skills_dir.join(name);
    let skill_md = skill_dir.join("SKILL.md");

    // 5a. M22 S3 versioning: compute the version to persist BEFORE overwriting.
    //     A re-save (an existing SKILL.md we can read) bumps the on-disk version
    //     by one; a first save keeps the incoming content's declared version
    //     (default 1). An existing file with unparseable frontmatter is treated
    //     as version 1 so a re-save still advances to 2.
    let target_version = match fs::read_to_string(&skill_md) {
        Ok(existing) => {
            let on_disk = frontmatter::parse(&existing)
                .map(|f| f.version())
                .unwrap_or(1);
            on_disk.saturating_add(1)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => fm.version(),
        Err(e) => return Err(SkillError::io(&skill_md, e)),
    };
    let versioned = set_frontmatter_version(content, target_version);

    fs::create_dir_all(&skill_dir).map_err(|e| SkillError::io(&skill_dir, e))?;
    fs::write(&skill_md, &versioned).map_err(|e| SkillError::io(&skill_md, e))?;
    Ok(skill_dir)
}

/// A saved skill as surfaced by [`list_group_skills`] (and, in the container,
/// the `list_skills` MCP tool): the kebab-case name, the effective
/// [`Frontmatter::version`], and the one-line description. Serializable so the
/// listing can be handed straight to a JSON tool result (M22 S3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillListing {
    /// Kebab-case skill name (equal to the on-disk directory slug).
    pub name: String,
    /// Effective skill version (frontmatter `version`, default 1).
    pub version: u32,
    /// One-line skill description from the frontmatter.
    pub description: String,
}

/// M22 S3: enumerate the skills saved in a group's per-group skills override
/// directory. Scans each `<group_skills_dir>/<slug>/SKILL.md`, parses its
/// frontmatter, and returns one [`SkillListing`] per valid skill (name +
/// effective version + description), sorted by name.
///
/// This is the host-side / crate-level list surface (the in-container
/// `list_skills` MCP tool reads the per-session skills catalogue instead). A
/// missing `group_skills_dir` yields an empty list — a group that has never
/// saved a skill is not an error. An individual skill whose `SKILL.md` is
/// missing, has unparseable frontmatter, or whose frontmatter `name` does not
/// match its directory slug is skipped, so one malformed skill never breaks
/// the whole listing.
///
/// # Errors
/// - [`SkillError::Io`] if the directory exists but cannot be read.
pub fn list_group_skills(group_skills_dir: &Path) -> Result<Vec<SkillListing>, SkillError> {
    let read = match fs::read_dir(group_skills_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SkillError::io(group_skills_dir, e)),
    };

    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| SkillError::io(group_skills_dir, e))?;
        // Skills are directories; skip stray files.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let slug = entry.file_name().to_string_lossy().into_owned();
        let skill_md = entry.path().join("SKILL.md");
        let Ok(content) = fs::read_to_string(&skill_md) else {
            continue; // no SKILL.md (or unreadable) — not a valid skill dir.
        };
        let Ok(fm) = frontmatter::parse(&content) else {
            continue; // malformed frontmatter — skip rather than fail the list.
        };
        // Enforce the same `name == dir` invariant discovery applies.
        if fm.name != slug {
            continue;
        }
        let version = fm.version();
        out.push(SkillListing {
            name: fm.name,
            version,
            description: fm.description,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// M22 S3: return `content` with its frontmatter `version:` set to `version`.
///
/// Replaces any existing top-level `version:` line in the YAML frontmatter and,
/// when none is present, inserts one just before the closing `---` delimiter.
/// Every other frontmatter line and the entire markdown body are preserved
/// verbatim (a leading BOM and CRLF line endings are respected). `content` is
/// assumed to have already passed [`validate_skill_content`], so it carries a
/// well-formed opening + closing `---`; if the delimiters are somehow absent
/// the content is returned unchanged.
fn set_frontmatter_version(content: &str, version: u32) -> String {
    let had_bom = content.starts_with('\u{feff}');
    let work = content.strip_prefix('\u{feff}').unwrap_or(content);

    // Detect the opening delimiter's line ending (`\n` vs `\r\n`).
    let crlf = work.starts_with("---\r\n");
    if !crlf && !work.starts_with("---\n") {
        return content.to_string();
    }
    let nl = if crlf { "\r\n" } else { "\n" };

    let lines: Vec<&str> = work.split_inclusive('\n').collect();
    // Find the closing `---` on its own line (after the opening at index 0).
    let Some(close_idx) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, l)| l.trim_end_matches(['\n', '\r']) == "---")
        .map(|(i, _)| i)
    else {
        return content.to_string();
    };

    let mut out = String::with_capacity(content.len() + 16);
    if had_bom {
        out.push('\u{feff}');
    }
    out.push_str(lines[0]); // opening `---` line (keeps its newline).
    // Copy the frontmatter body, dropping any existing TOP-LEVEL `version:`
    // line (a nested/indented `version:` under some other key is left alone).
    for line in &lines[1..close_idx] {
        if line.starts_with("version:") {
            continue;
        }
        out.push_str(line);
    }
    out.push_str("version: ");
    out.push_str(&version.to_string());
    out.push_str(nl);
    // Closing delimiter and the entire body, verbatim.
    for line in &lines[close_idx..] {
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{SkillRegistry, SkillSource};
    use copperclaw_types::AgentGroupId;
    use tempfile::TempDir;

    const VALID: &str =
        "---\nname: my-skill\ndescription: A reusable procedure\n---\n# Steps\ndo the thing\n";

    const VALID_GREET: &str =
        "---\nname: greet\ndescription: Say hello nicely\n---\n# Greet\nSay hi.\n";

    #[test]
    fn writes_valid_skill_and_returns_dir() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("groups").join("ag").join("skills");
        let dir = save_group_skill(&dest, &[], "my-skill", VALID).unwrap();
        assert_eq!(dir, dest.join("my-skill"));
        let body = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        // M22 S3: the persisted skill carries a version (default 1 on a first
        // save) and preserves the original name/description/body verbatim.
        let fm = frontmatter::parse(&body).unwrap();
        assert_eq!(fm.version(), 1);
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "A reusable procedure");
        assert!(body.contains("do the thing"));
    }

    #[test]
    fn saved_skill_is_discovered_by_scan() {
        // The whole point of A4: the write lands where discovery scans, so the
        // next spawn picks it up.
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        let group = td.path().join("groups").join("ag").join("skills");

        save_group_skill(&group, &[], "my-skill", VALID).unwrap();

        let gid = AgentGroupId::new();
        let reg = SkillRegistry::scan(&global, Some((gid, &group))).unwrap();
        let skill = reg.get("my-skill").expect("saved skill discovered");
        assert_eq!(skill.description, "A reusable procedure");
        assert_eq!(skill.source, SkillSource::Group(gid));
    }

    #[test]
    fn re_save_updates_body() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        save_group_skill(&dest, &[], "my-skill", VALID).unwrap();
        let updated =
            "---\nname: my-skill\ndescription: A reusable procedure\n---\n# Steps\nrevised\n";
        let dir = save_group_skill(&dest, &[], "my-skill", updated).unwrap();
        // The body is updated (re-save overwrites), and the version bumped.
        let body = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(body.contains("revised"));
        assert!(!body.contains("do the thing"));
        assert_eq!(frontmatter::parse(&body).unwrap().version(), 2);
    }

    // ── M22 S3: versioning + listing ────────────────────────────────────

    #[test]
    fn first_save_defaults_to_version_one() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        let dir = save_group_skill(&dest, &[], "my-skill", VALID).unwrap();
        let fm = frontmatter::parse(&fs::read_to_string(dir.join("SKILL.md")).unwrap()).unwrap();
        assert_eq!(fm.version(), 1);
    }

    #[test]
    fn first_save_honours_declared_version() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        let declared = "---\nname: my-skill\ndescription: d\nversion: 5\n---\nbody\n";
        let dir = save_group_skill(&dest, &[], "my-skill", declared).unwrap();
        let fm = frontmatter::parse(&fs::read_to_string(dir.join("SKILL.md")).unwrap()).unwrap();
        assert_eq!(fm.version(), 5);
    }

    #[test]
    fn re_save_bumps_version_monotonically() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        for expected in 1..=4 {
            let dir = save_group_skill(&dest, &[], "my-skill", VALID).unwrap();
            let fm =
                frontmatter::parse(&fs::read_to_string(dir.join("SKILL.md")).unwrap()).unwrap();
            assert_eq!(
                fm.version(),
                expected,
                "save #{expected} should be version {expected}"
            );
        }
    }

    #[test]
    fn re_save_bump_ignores_stale_declared_version() {
        // The bump is driven by what's ON DISK, not what the caller re-declares
        // — a caller that keeps sending `version: 1` still advances the skill.
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        let v1 = "---\nname: my-skill\ndescription: d\nversion: 1\n---\nbody\n";
        save_group_skill(&dest, &[], "my-skill", v1).unwrap();
        let dir = save_group_skill(&dest, &[], "my-skill", v1).unwrap();
        let fm = frontmatter::parse(&fs::read_to_string(dir.join("SKILL.md")).unwrap()).unwrap();
        assert_eq!(fm.version(), 2);
    }

    #[test]
    fn set_frontmatter_version_replaces_existing_line() {
        let input = "---\nname: x\ndescription: y\nversion: 3\n---\nbody\n";
        let out = set_frontmatter_version(input, 9);
        assert_eq!(frontmatter::parse(&out).unwrap().version(), 9);
        // No stale duplicate `version:` line survives.
        assert_eq!(out.matches("version:").count(), 1);
        assert!(out.contains("body"));
    }

    #[test]
    fn set_frontmatter_version_inserts_when_absent() {
        let input = "---\nname: x\ndescription: y\n---\nbody\n";
        let out = set_frontmatter_version(input, 2);
        assert_eq!(frontmatter::parse(&out).unwrap().version(), 2);
        assert!(out.contains("name: x"));
        assert!(out.contains("description: y"));
    }

    #[test]
    fn set_frontmatter_version_handles_crlf_and_bom() {
        let input = "\u{feff}---\r\nname: x\r\ndescription: y\r\n---\r\nbody\r\n";
        let out = set_frontmatter_version(input, 4);
        assert!(out.starts_with('\u{feff}'));
        assert_eq!(frontmatter::parse(&out).unwrap().version(), 4);
    }

    #[test]
    fn list_group_skills_missing_dir_is_empty() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("never-created");
        assert!(list_group_skills(&dest).unwrap().is_empty());
    }

    #[test]
    fn list_group_skills_returns_saved_skills_sorted() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        save_group_skill(
            &dest,
            &[],
            "zed",
            "---\nname: zed\ndescription: the zed skill\n---\nb\n",
        )
        .unwrap();
        save_group_skill(
            &dest,
            &[],
            "alpha",
            "---\nname: alpha\ndescription: the alpha skill\n---\nb\n",
        )
        .unwrap();
        // Bump alpha so its version differs from a fresh default.
        save_group_skill(
            &dest,
            &[],
            "alpha",
            "---\nname: alpha\ndescription: the alpha skill\n---\nb2\n",
        )
        .unwrap();

        let listing = list_group_skills(&dest).unwrap();
        assert_eq!(
            listing,
            vec![
                SkillListing {
                    name: "alpha".into(),
                    version: 2,
                    description: "the alpha skill".into(),
                },
                SkillListing {
                    name: "zed".into(),
                    version: 1,
                    description: "the zed skill".into(),
                },
            ]
        );
    }

    #[test]
    fn list_group_skills_skips_malformed_and_stray_entries() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        save_group_skill(
            &dest,
            &[],
            "good",
            "---\nname: good\ndescription: d\n---\nb\n",
        )
        .unwrap();
        // A directory with a malformed SKILL.md.
        let bad = dest.join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("SKILL.md"), "no frontmatter here\n").unwrap();
        // A directory whose frontmatter name mismatches its slug.
        let mism = dest.join("mismatch");
        fs::create_dir_all(&mism).unwrap();
        fs::write(
            mism.join("SKILL.md"),
            "---\nname: other\ndescription: d\n---\nb\n",
        )
        .unwrap();
        // A stray file (not a directory).
        fs::write(dest.join("loose.txt"), "x").unwrap();

        let listing = list_group_skills(&dest).unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].name, "good");
    }

    #[test]
    fn author_list_reload_round_trip() {
        // The S3 integration round-trip: author a skill, list it, reload it
        // and confirm the persisted version survives + a re-save bumps it in
        // the listing.
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");

        // Author.
        let dir = save_group_skill(&dest, &[], "greet", VALID_GREET).unwrap();

        // List.
        let listing = list_group_skills(&dest).unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].name, "greet");
        assert_eq!(listing[0].version, 1);

        // Reload (parse the persisted SKILL.md back).
        let reloaded =
            frontmatter::parse(&fs::read_to_string(dir.join("SKILL.md")).unwrap()).unwrap();
        assert_eq!(reloaded.version(), 1);
        assert_eq!(reloaded.name, "greet");

        // Re-author → the listing reflects the bumped version.
        save_group_skill(&dest, &[], "greet", VALID_GREET).unwrap();
        let listing = list_group_skills(&dest).unwrap();
        assert_eq!(listing[0].version, 2);
    }

    #[test]
    fn invalid_frontmatter_is_refused_with_precise_error() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        let err = save_group_skill(&dest, &[], "my-skill", "no frontmatter here\n").unwrap_err();
        match err {
            SkillError::Frontmatter(msg) => assert!(msg.contains("opening"), "got: {msg}"),
            other => panic!("expected Frontmatter, got {other:?}"),
        }
        // Nothing was written.
        assert!(!dest.join("my-skill").exists());
    }

    #[test]
    fn missing_description_is_refused() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        let err = save_group_skill(&dest, &[], "my-skill", "---\nname: my-skill\n---\nbody\n")
            .unwrap_err();
        assert!(matches!(err, SkillError::Frontmatter(_)));
    }

    #[test]
    fn name_mismatch_is_refused() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        let err = save_group_skill(&dest, &[], "requested-name", VALID).unwrap_err();
        match err {
            SkillError::InvalidName(msg) => {
                assert!(msg.contains("does not match"), "got: {msg}");
                assert!(msg.contains("requested-name"));
            }
            other => panic!("expected InvalidName, got {other:?}"),
        }
    }

    #[test]
    fn non_kebab_name_is_refused() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("skills");
        let content = "---\nname: Bad_Name\ndescription: d\n---\nb\n";
        let err = save_group_skill(&dest, &[], "Bad_Name", content).unwrap_err();
        assert!(matches!(err, SkillError::InvalidName(_)));
    }

    #[test]
    fn destination_escaping_allowed_root_is_refused() {
        let td = TempDir::new().unwrap();
        // The allowed root is a sibling; the destination is outside it.
        let allowed = td.path().join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let outside = td.path().join("outside").join("skills");
        let err = save_group_skill(&outside, std::slice::from_ref(&allowed), "my-skill", VALID)
            .unwrap_err();
        assert!(matches!(err, SkillError::EscapedRoot { .. }));
    }

    #[test]
    fn destination_inside_allowed_root_is_accepted() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("groups");
        fs::create_dir_all(&root).unwrap();
        let dest = root.join("ag").join("skills");
        let canonical_root = root.canonicalize().unwrap();
        let dir = save_group_skill(
            &dest,
            std::slice::from_ref(&canonical_root),
            "my-skill",
            VALID,
        )
        .unwrap();
        assert!(dir.join("SKILL.md").is_file());
    }
}
