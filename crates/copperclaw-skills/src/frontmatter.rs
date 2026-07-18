//! YAML frontmatter parsing for `SKILL.md` files.
//!
//! A skill markdown file must start with a YAML frontmatter block delimited by
//! `---` lines on their own:
//!
//! ```text
//! ---
//! name: my-skill
//! description: One-line description
//! allowed-tools: [Read, Bash]   # optional
//! ---
//!
//! # Body
//! ```
//!
//! Only the frontmatter is parsed here; the markdown body is ignored.

use serde::Deserialize;

use crate::error::SkillError;

/// Parsed frontmatter fields. Field names mirror the markdown convention
/// (`allowed-tools` becomes `allowed_tools` via serde rename).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Frontmatter {
    pub name: String,
    pub description: String,
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<Vec<String>>,
    // M22 S3: optional monotonic skill version. Absent in every one of the 41
    // shipped skills, so it is `#[serde(default)]` (parses to `None`) and the
    // effective value defaults to 1 via [`Frontmatter::version`] — existing
    // skills keep parsing unchanged. `save_skill` bumps this on every re-save
    // so an agent-authored skill carries a monotonic revision. Kept localized
    // (this field + the `version()` accessor below) so card S4's `tools:`
    // addition rebases cleanly on top.
    #[serde(default)]
    pub version: Option<u32>,
}

impl Frontmatter {
    /// M22 S3: the effective skill version. The `version` frontmatter field is
    /// optional; a skill without it (every one of the 41 shipped skills, and
    /// any first-time agent-authored skill that omits it) defaults to version
    /// 1. `save_skill` writes an explicit bumped version on re-save.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version.unwrap_or(1)
    }
}

/// Parse the YAML frontmatter from a `SKILL.md` body.
///
/// # Errors
/// - [`SkillError::Frontmatter`] if the frontmatter delimiters are missing,
///   the YAML is malformed, or required fields (`name`, `description`) are
///   missing or blank.
pub fn parse(input: &str) -> Result<Frontmatter, SkillError> {
    let body = strip_bom(input);
    let rest = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))
        .ok_or_else(|| SkillError::Frontmatter("missing opening `---` delimiter".to_string()))?;

    // Find the closing `---` on its own line.
    let end = find_closing_delimiter(rest)
        .ok_or_else(|| SkillError::Frontmatter("missing closing `---` delimiter".to_string()))?;

    let yaml = &rest[..end];
    let fm: Frontmatter = serde_yaml::from_str(yaml)
        .map_err(|e| SkillError::Frontmatter(format!("invalid YAML: {e}")))?;

    if fm.name.trim().is_empty() {
        return Err(SkillError::Frontmatter("`name` is required".to_string()));
    }
    if fm.description.trim().is_empty() {
        return Err(SkillError::Frontmatter(
            "`description` is required".to_string(),
        ));
    }

    Ok(fm)
}

/// Return the markdown body portion of a `SKILL.md` (everything after the
/// closing `---` delimiter), or the unchanged input when no frontmatter
/// is present. Used by callers that want to splice the body into a
/// larger document (e.g. an agent system prompt) without re-parsing the
/// frontmatter.
///
/// Leading newlines immediately following the closing delimiter are
/// trimmed so the body starts at the first meaningful character.
pub fn skip_frontmatter(input: &str) -> &str {
    let body = strip_bom(input);
    let Some(rest) = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))
    else {
        return input;
    };
    let Some(end) = find_closing_delimiter(rest) else {
        return input;
    };
    let after = &rest[end..];
    // Consume the closing `---` line itself.
    let after = after
        .strip_prefix("---\n")
        .or_else(|| after.strip_prefix("---\r\n"))
        .unwrap_or(after);
    after.trim_start_matches(['\n', '\r'])
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

fn find_closing_delimiter(s: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_frontmatter() {
        let input = "---\nname: my-skill\ndescription: A skill\n---\n# body\n";
        let fm = parse(input).unwrap();
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "A skill");
        assert!(fm.allowed_tools.is_none());
    }

    #[test]
    fn parses_allowed_tools() {
        let input = "---\nname: s\ndescription: d\nallowed-tools: [Read, Bash]\n---\n";
        let fm = parse(input).unwrap();
        assert_eq!(
            fm.allowed_tools,
            Some(vec!["Read".to_string(), "Bash".to_string()])
        );
    }

    #[test]
    fn handles_crlf_line_endings() {
        let input = "---\r\nname: x\r\ndescription: y\r\n---\r\nbody\r\n";
        let fm = parse(input).unwrap();
        assert_eq!(fm.name, "x");
        assert_eq!(fm.description, "y");
    }

    #[test]
    fn handles_bom_prefix() {
        let input = "\u{feff}---\nname: x\ndescription: y\n---\n";
        let fm = parse(input).unwrap();
        assert_eq!(fm.name, "x");
    }

    #[test]
    fn missing_opening_delim_is_error() {
        let input = "name: x\ndescription: y\n";
        let err = parse(input).unwrap_err();
        assert!(matches!(err, SkillError::Frontmatter(_)));
        assert!(err.to_string().contains("opening"));
    }

    #[test]
    fn missing_closing_delim_is_error() {
        let input = "---\nname: x\ndescription: y\n";
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("closing"));
    }

    #[test]
    fn malformed_yaml_is_error() {
        let input = "---\nname: [unterminated\n---\n";
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("invalid YAML"));
    }

    #[test]
    fn missing_description_is_error() {
        let input = "---\nname: x\n---\n";
        let err = parse(input).unwrap_err();
        assert!(
            err.to_string().contains("invalid YAML") || err.to_string().contains("description")
        );
    }

    #[test]
    fn blank_description_is_error() {
        let input = "---\nname: x\ndescription: \"\"\n---\n";
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("description"));
    }

    #[test]
    fn missing_name_is_error() {
        let input = "---\ndescription: y\n---\n";
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("invalid YAML") || err.to_string().contains("name"));
    }

    #[test]
    fn blank_name_is_error() {
        let input = "---\nname: \"   \"\ndescription: y\n---\n";
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn skip_frontmatter_strips_yaml_and_leading_blank() {
        let input = "---\nname: x\ndescription: y\n---\n\n# body\nhello\n";
        assert_eq!(skip_frontmatter(input), "# body\nhello\n");
    }

    #[test]
    fn skip_frontmatter_handles_crlf() {
        let input = "---\r\nname: x\r\ndescription: y\r\n---\r\n# body\r\n";
        assert_eq!(skip_frontmatter(input), "# body\r\n");
    }

    #[test]
    fn skip_frontmatter_handles_bom() {
        let input = "\u{feff}---\nname: x\ndescription: y\n---\nbody\n";
        assert_eq!(skip_frontmatter(input), "body\n");
    }

    #[test]
    fn skip_frontmatter_returns_input_when_no_frontmatter() {
        let input = "no frontmatter here\n# header\n";
        assert_eq!(skip_frontmatter(input), input);
    }

    #[test]
    fn skip_frontmatter_returns_input_when_unterminated() {
        let input = "---\nname: x\nno closing delim";
        assert_eq!(skip_frontmatter(input), input);
    }

    #[test]
    fn frontmatter_clone_eq() {
        let a = Frontmatter {
            name: "n".into(),
            description: "d".into(),
            allowed_tools: None,
            version: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── M22 S3: version field ────────────────────────────────────────────

    #[test]
    fn version_defaults_to_one_when_absent() {
        // Every one of the 41 shipped skills omits `version`; they must keep
        // parsing and default to version 1.
        let input = "---\nname: x\ndescription: y\n---\n# body\n";
        let fm = parse(input).unwrap();
        assert!(fm.version.is_none());
        assert_eq!(fm.version(), 1);
    }

    #[test]
    fn version_is_parsed_when_present() {
        let input = "---\nname: x\ndescription: y\nversion: 7\n---\n";
        let fm = parse(input).unwrap();
        assert_eq!(fm.version, Some(7));
        assert_eq!(fm.version(), 7);
    }

    #[test]
    fn version_coexists_with_allowed_tools() {
        let input = "---\nname: x\ndescription: y\nallowed-tools: [Read]\nversion: 3\n---\n";
        let fm = parse(input).unwrap();
        assert_eq!(fm.allowed_tools, Some(vec!["Read".to_string()]));
        assert_eq!(fm.version(), 3);
    }
}
