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
use std::path::{Path, PathBuf};

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
    validate_skill_content(name, content)?;

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
    fs::create_dir_all(&skill_dir).map_err(|e| SkillError::io(&skill_dir, e))?;
    let skill_md = skill_dir.join("SKILL.md");
    fs::write(&skill_md, content).map_err(|e| SkillError::io(&skill_md, e))?;
    Ok(skill_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{SkillRegistry, SkillSource};
    use copperclaw_types::AgentGroupId;
    use tempfile::TempDir;

    const VALID: &str = "---\nname: my-skill\ndescription: A reusable procedure\n---\n# Steps\ndo the thing\n";

    #[test]
    fn writes_valid_skill_and_returns_dir() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("groups").join("ag").join("skills");
        let dir = save_group_skill(&dest, &[], "my-skill", VALID).unwrap();
        assert_eq!(dir, dest.join("my-skill"));
        let body = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert_eq!(body, VALID);
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
        assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), updated);
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
        let err =
            save_group_skill(&dest, &[], "my-skill", "---\nname: my-skill\n---\nbody\n").unwrap_err();
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
        let err =
            save_group_skill(&outside, std::slice::from_ref(&allowed), "my-skill", VALID)
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
        let dir =
            save_group_skill(&dest, std::slice::from_ref(&canonical_root), "my-skill", VALID)
                .unwrap();
        assert!(dir.join("SKILL.md").is_file());
    }
}
