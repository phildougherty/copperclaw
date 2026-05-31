//! Error type for the skills crate.

use std::path::PathBuf;

use thiserror::Error;

/// All errors surfaced by skill discovery and materialization.
#[derive(Debug, Error)]
pub enum SkillError {
    /// The YAML frontmatter is missing, malformed, or missing required fields.
    #[error("invalid frontmatter: {0}")]
    Frontmatter(String),

    /// The skill directory has no `SKILL.md` file.
    #[error("missing SKILL.md in {0}")]
    MissingSkillMd(PathBuf),

    /// An I/O operation failed while reading or materializing a skill.
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The skill's `name` field does not satisfy the kebab-case constraint or
    /// does not match the directory name.
    #[error("invalid skill name: {0}")]
    InvalidName(String),

    /// During `materialize`, the canonicalized source path escapes the set of
    /// configured skill roots. This is a defense-in-depth check against
    /// malicious group overrides.
    #[error("symlink target {target} escapes allowed roots")]
    EscapedRoot { target: PathBuf },
}

impl SkillError {
    /// Convenience constructor for an io error tagged with the offending path.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        SkillError::Io {
            path: path.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn frontmatter_display() {
        let e = SkillError::Frontmatter("bad yaml".into());
        assert_eq!(e.to_string(), "invalid frontmatter: bad yaml");
    }

    #[test]
    fn missing_skill_md_display() {
        let e = SkillError::MissingSkillMd(PathBuf::from("/x"));
        assert!(e.to_string().contains("missing SKILL.md"));
        assert!(e.to_string().contains("/x"));
    }

    #[test]
    fn io_constructor_records_path() {
        let e = SkillError::io("/a/b", io::Error::other("oops"));
        let s = e.to_string();
        assert!(s.contains("/a/b"));
        assert!(s.contains("oops"));
        match e {
            SkillError::Io { path, .. } => assert_eq!(path, PathBuf::from("/a/b")),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn invalid_name_display() {
        let e = SkillError::InvalidName("BadName".into());
        assert_eq!(e.to_string(), "invalid skill name: BadName");
    }

    #[test]
    fn escaped_root_display() {
        let e = SkillError::EscapedRoot {
            target: PathBuf::from("/etc/passwd"),
        };
        assert!(e.to_string().contains("/etc/passwd"));
        assert!(e.to_string().contains("escapes"));
    }
}
