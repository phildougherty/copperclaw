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
        .ok_or_else(|| {
            SkillError::Frontmatter("missing opening `---` delimiter".to_string())
        })?;

    // Find the closing `---` on its own line.
    let end = find_closing_delimiter(rest).ok_or_else(|| {
        SkillError::Frontmatter("missing closing `---` delimiter".to_string())
    })?;

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
        assert!(err.to_string().contains("invalid YAML") || err.to_string().contains("description"));
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
    fn frontmatter_clone_eq() {
        let a = Frontmatter {
            name: "n".into(),
            description: "d".into(),
            allowed_tools: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
