//! Materialize a set of skills into a destination directory by creating
//! symlinks.
//!
//! Symlink layout:
//!
//! ```text
//! <dest>/<skill_id>  -> <skill.dir>
//! ```
//!
//! Each link is created independently. If creating one link fails the
//! function records the error but continues with the remaining skills, so
//! the caller can surface every problem at once.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::error::SkillError;
use crate::registry::Skill;

/// Per-skill outcome of [`materialize`]. A successful entry has
/// `error: None`; a failed entry carries the [`SkillError`].
#[derive(Debug)]
pub struct MaterializeOutcome {
    pub skill: String,
    pub link: PathBuf,
    pub error: Option<SkillError>,
}

/// Aggregate result of [`materialize`].
#[derive(Debug, Default)]
pub struct MaterializeReport {
    pub outcomes: Vec<MaterializeOutcome>,
}

impl MaterializeReport {
    /// All errors encountered. Empty on full success.
    pub fn errors(&self) -> Vec<&SkillError> {
        self.outcomes
            .iter()
            .filter_map(|o| o.error.as_ref())
            .collect()
    }

    /// Whether every skill was materialized without error.
    pub fn is_ok(&self) -> bool {
        self.outcomes.iter().all(|o| o.error.is_none())
    }
}

/// Create symlinks for each skill at `<dest>/<skill_id>`.
///
/// If `dest` does not exist it is created.
///
/// Per-skill behavior:
/// - If no entry exists at `<dest>/<skill_id>`, a new symlink is created.
/// - If a symlink already exists pointing to `skill.dir`, it is left alone
///   (idempotent re-materialize).
/// - If a symlink exists pointing somewhere else, it is replaced.
/// - If a regular file or directory occupies the spot, that's an error.
///
/// `allowed_roots`, if non-empty, restricts which canonical paths skill
/// directories may resolve under. Any skill whose canonical `dir` does not
/// fall within at least one of those roots is rejected with
/// [`SkillError::EscapedRoot`]. Pass an empty slice to disable the check
/// (e.g. in tests using `tempfile` paths under `/tmp`).
///
/// # Errors
/// This function returns `Err` only if it cannot create `dest` itself or
/// cannot canonicalize the destination. Per-skill errors are collected into
/// the returned [`MaterializeReport`].
pub fn materialize(
    skills: &[Skill],
    dest: &Path,
    allowed_roots: &[PathBuf],
) -> Result<MaterializeReport, SkillError> {
    fs::create_dir_all(dest).map_err(|e| SkillError::io(dest, e))?;

    // Canonicalize the allowed roots once.
    let canonical_roots: Vec<PathBuf> = allowed_roots
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();

    let mut report = MaterializeReport::default();
    for skill in skills {
        let link = dest.join(skill.id.as_str());
        let err = link_one(skill, &link, &canonical_roots).err();
        report.outcomes.push(MaterializeOutcome {
            skill: skill.name.clone(),
            link,
            error: err,
        });
    }
    Ok(report)
}

fn link_one(
    skill: &Skill,
    link: &Path,
    allowed_roots: &[PathBuf],
) -> Result<(), SkillError> {
    let canonical = skill
        .dir
        .canonicalize()
        .map_err(|e| SkillError::io(&skill.dir, e))?;

    if !allowed_roots.is_empty() && !is_under_any(&canonical, allowed_roots) {
        return Err(SkillError::EscapedRoot { target: canonical });
    }

    match fs::symlink_metadata(link) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                let current = fs::read_link(link).map_err(|e| SkillError::io(link, e))?;
                // Compare canonical targets so a relative or stale link is
                // detected.
                let current_canon = current
                    .canonicalize()
                    .unwrap_or_else(|_| current.clone());
                if current_canon == canonical {
                    return Ok(());
                }
                fs::remove_file(link).map_err(|e| SkillError::io(link, e))?;
                symlink(&canonical, link).map_err(|e| SkillError::io(link, e))?;
                Ok(())
            } else {
                Err(SkillError::io(
                    link,
                    std::io::Error::other(format!(
                        "destination path exists and is not a symlink: {link:?}"
                    )),
                ))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            symlink(&canonical, link).map_err(|e| SkillError::io(link, e))?;
            Ok(())
        }
        Err(e) => Err(SkillError::io(link, e)),
    }
}

fn is_under_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink as unix_symlink;

    use tempfile::TempDir;

    use super::*;
    use crate::registry::{Skill, SkillId, SkillSource};

    fn mk_skill(name: &str, dir: &Path) -> Skill {
        Skill {
            id: SkillId(name.to_string()),
            name: name.to_string(),
            description: "d".to_string(),
            dir: dir.to_path_buf(),
            allowed_tools: None,
            source: SkillSource::Global,
        }
    }

    fn write_skill_dir(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "stub").unwrap();
        dir
    }

    #[test]
    fn report_helpers_on_empty() {
        let r = MaterializeReport::default();
        assert!(r.is_ok());
        assert!(r.errors().is_empty());
    }

    #[test]
    fn materialize_creates_symlinks() {
        let td = TempDir::new().unwrap();
        let src_root = td.path().join("src");
        fs::create_dir_all(&src_root).unwrap();
        let a_dir = write_skill_dir(&src_root, "alpha");
        let b_dir = write_skill_dir(&src_root, "beta");

        let dest = td.path().join("dest");
        let skills = vec![mk_skill("alpha", &a_dir), mk_skill("beta", &b_dir)];

        let report = materialize(&skills, &dest, &[]).unwrap();
        assert!(report.is_ok(), "errors: {:?}", report.errors());

        let link_a = dest.join("alpha");
        let link_b = dest.join("beta");
        assert!(fs::symlink_metadata(&link_a).unwrap().file_type().is_symlink());
        assert!(fs::symlink_metadata(&link_b).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&link_a).unwrap(), a_dir.canonicalize().unwrap());
    }

    #[test]
    fn materialize_creates_dest_dir_if_missing() {
        let td = TempDir::new().unwrap();
        let src = write_skill_dir(td.path(), "alpha");
        let dest = td.path().join("nested").join("dest");
        let report = materialize(&[mk_skill("alpha", &src)], &dest, &[]).unwrap();
        assert!(report.is_ok());
        assert!(dest.join("alpha").exists());
    }

    #[test]
    fn materialize_is_idempotent() {
        let td = TempDir::new().unwrap();
        let src = write_skill_dir(td.path(), "alpha");
        let dest = td.path().join("dest");

        let skills = vec![mk_skill("alpha", &src)];
        let r1 = materialize(&skills, &dest, &[]).unwrap();
        assert!(r1.is_ok());
        let r2 = materialize(&skills, &dest, &[]).unwrap();
        assert!(r2.is_ok());

        let link = dest.join("alpha");
        assert_eq!(fs::read_link(&link).unwrap(), src.canonicalize().unwrap());
    }

    #[test]
    fn materialize_replaces_stale_target() {
        let td = TempDir::new().unwrap();
        let real_src = write_skill_dir(td.path(), "alpha");
        let stale_src = write_skill_dir(td.path(), "stale-target");
        let dest = td.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        unix_symlink(&stale_src, dest.join("alpha")).unwrap();

        let skills = vec![mk_skill("alpha", &real_src)];
        let report = materialize(&skills, &dest, &[]).unwrap();
        assert!(report.is_ok(), "errors: {:?}", report.errors());
        assert_eq!(
            fs::read_link(dest.join("alpha")).unwrap(),
            real_src.canonicalize().unwrap()
        );
    }

    #[test]
    fn materialize_rejects_non_symlink_at_destination() {
        let td = TempDir::new().unwrap();
        let src = write_skill_dir(td.path(), "alpha");
        let dest = td.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        // Pre-create a regular directory at the spot the link should occupy.
        fs::create_dir(dest.join("alpha")).unwrap();

        let report = materialize(&[mk_skill("alpha", &src)], &dest, &[]).unwrap();
        assert!(!report.is_ok());
        let errs = report.errors();
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], SkillError::Io { .. }));
    }

    #[test]
    fn materialize_continues_across_skills_on_error() {
        let td = TempDir::new().unwrap();
        let good = write_skill_dir(td.path(), "good");
        let bad_dir = td.path().join("missing-dir");

        let dest = td.path().join("dest");
        let skills = vec![
            mk_skill("bad", &bad_dir),
            mk_skill("good", &good),
        ];
        let report = materialize(&skills, &dest, &[]).unwrap();
        assert_eq!(report.outcomes.len(), 2);
        // Good one succeeded.
        let good_outcome = report
            .outcomes
            .iter()
            .find(|o| o.skill == "good")
            .unwrap();
        assert!(good_outcome.error.is_none());
        // Bad one errored.
        let bad_outcome = report
            .outcomes
            .iter()
            .find(|o| o.skill == "bad")
            .unwrap();
        assert!(bad_outcome.error.is_some());
        assert!(!report.is_ok());
    }

    #[test]
    fn materialize_rejects_path_escape() {
        let td = TempDir::new().unwrap();
        let inside_root = td.path().join("roots");
        fs::create_dir_all(&inside_root).unwrap();
        // The skill lives OUTSIDE the allowed root.
        let outside = write_skill_dir(td.path(), "outside-skill");

        let dest = td.path().join("dest");
        let skills = vec![mk_skill("outside-skill", &outside)];
        let report = materialize(&skills, &dest, &[inside_root]).unwrap();
        let errs = report.errors();
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], SkillError::EscapedRoot { .. }));
    }

    #[test]
    fn materialize_accepts_path_inside_allowed_root() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("roots");
        fs::create_dir_all(&root).unwrap();
        let src = write_skill_dir(&root, "alpha");
        let dest = td.path().join("dest");

        let report = materialize(
            &[mk_skill("alpha", &src)],
            &dest,
            &[root.canonicalize().unwrap()],
        )
        .unwrap();
        assert!(report.is_ok(), "errors: {:?}", report.errors());
    }

    #[test]
    fn materialize_returns_err_if_dest_uncreatable() {
        let td = TempDir::new().unwrap();
        // Create a file where the dest dir would go; create_dir_all will fail.
        let blocker = td.path().join("blocker");
        fs::write(&blocker, b"i am a file").unwrap();
        let dest = blocker.join("dest"); // can't mkdir under a file

        let err = materialize(&[], &dest, &[]).unwrap_err();
        assert!(matches!(err, SkillError::Io { .. }));
    }

    #[test]
    fn empty_skill_list_is_ok() {
        let td = TempDir::new().unwrap();
        let dest = td.path().join("dest");
        let report = materialize(&[], &dest, &[]).unwrap();
        assert!(report.is_ok());
        assert!(report.outcomes.is_empty());
        assert!(dest.is_dir());
    }
}
