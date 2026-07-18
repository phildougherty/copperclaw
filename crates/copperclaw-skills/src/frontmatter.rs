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
    // M22 S4: optional `tools:` allowlist — a companion to `allowed-tools:`.
    // Both declare the tool surface a skill needs, so an active skill can scope
    // dispatch to just those tools. Reserved-but-unused before S4; now parsed
    // and, in [`parse`], folded into `allowed_tools` (union, order-preserving,
    // de-duplicated) so the whole existing enforcement pipeline — registry
    // `Skill::allowed_tool_names` → skills catalogue → `load_skill` → the
    // runner's `ToolPolicy::with_active_skill` dispatch gate — scopes on
    // `tools:` with no downstream change. Absent in every one of the 41 shipped
    // skills, so `#[serde(default)]` (parses to `None`); a skill declaring
    // neither key keeps `allowed_tools == None` and narrows nothing.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
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

    /// M22 S4: the skill's declared tool allowlist for active-skill
    /// narrowing — the union of the `allowed-tools:` and `tools:` frontmatter
    /// keys, in declaration order (`allowed-tools:` first) with duplicates
    /// removed. Returns `None` when the skill declares neither key (it imposes
    /// no tool scope — the group profile alone bounds it), which is the case
    /// for all 41 shipped skills.
    ///
    /// [`parse`] already folds `tools:` into `allowed_tools`, so for a parsed
    /// frontmatter this equals `allowed_tools`; the accessor recomputes the
    /// union so a directly-constructed `Frontmatter` (e.g. in a test) is
    /// handled the same way. The names are raw frontmatter names — callers that
    /// gate dispatch normalize them via [`crate::tool_names::normalize`].
    #[must_use]
    pub fn declared_tools(&self) -> Option<Vec<String>> {
        if self.allowed_tools.is_none() && self.tools.is_none() {
            return None;
        }
        let mut out: Vec<String> = Vec::new();
        for name in self
            .allowed_tools
            .iter()
            .flatten()
            .chain(self.tools.iter().flatten())
        {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        Some(out)
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
    let mut fm: Frontmatter = serde_yaml::from_str(yaml)
        .map_err(|e| SkillError::Frontmatter(format!("invalid YAML: {e}")))?;

    // M22 S4: fold the `tools:` allowlist into `allowed_tools` so downstream
    // consumers that read the `allowed_tools` field verbatim (registry
    // `Skill::allowed_tools` → `Skill::allowed_tool_names` → skills catalogue →
    // dispatch) enforce a `tools:`-declared scope without any change of their
    // own. Union, order-preserving (`allowed-tools:` entries first), de-duped.
    // A skill declaring neither key leaves `allowed_tools == None` (narrows
    // nothing) — preserving today's behaviour for the 41 shipped skills.
    if let Some(tools) = fm.tools.clone() {
        let merged = fm.allowed_tools.get_or_insert_with(Vec::new);
        for t in tools {
            if !merged.contains(&t) {
                merged.push(t);
            }
        }
    }

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
            tools: None,
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

    // ── M22 S4: `tools:` frontmatter ─────────────────────────────────────

    #[test]
    fn tools_absent_narrows_nothing() {
        // The fail-open case that keeps every shipped skill unchanged: no
        // `tools:` and no `allowed-tools:` means no tool scope at all.
        let input = "---\nname: x\ndescription: y\n---\n# body\n";
        let fm = parse(input).unwrap();
        assert!(fm.tools.is_none());
        assert!(fm.allowed_tools.is_none());
        assert!(fm.declared_tools().is_none());
    }

    #[test]
    fn tools_is_parsed_and_folded_into_allowed_tools() {
        // A skill declaring only `tools:` must scope dispatch: the fold puts
        // its entries into `allowed_tools`, which is the field the registry
        // (and thence the catalogue + dispatch gate) reads verbatim.
        let input = "---\nname: x\ndescription: y\ntools: [Read, Bash]\n---\n";
        let fm = parse(input).unwrap();
        assert_eq!(fm.tools, Some(vec!["Read".to_string(), "Bash".to_string()]));
        assert_eq!(
            fm.allowed_tools,
            Some(vec!["Read".to_string(), "Bash".to_string()]),
            "`tools:` must fold into `allowed_tools` so downstream enforces it"
        );
        assert_eq!(
            fm.declared_tools(),
            Some(vec!["Read".to_string(), "Bash".to_string()])
        );
    }

    #[test]
    fn tools_and_allowed_tools_union_without_duplicates() {
        // Both keys present: the effective scope is their union, `allowed-tools`
        // first, de-duplicated.
        let input =
            "---\nname: x\ndescription: y\nallowed-tools: [Read]\ntools: [Read, Grep]\n---\n";
        let fm = parse(input).unwrap();
        assert_eq!(
            fm.allowed_tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        assert_eq!(
            fm.declared_tools(),
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
    }

    #[test]
    fn tools_coexists_with_version() {
        let input = "---\nname: x\ndescription: y\ntools: [Read]\nversion: 4\n---\n";
        let fm = parse(input).unwrap();
        assert_eq!(fm.tools, Some(vec!["Read".to_string()]));
        assert_eq!(fm.allowed_tools, Some(vec!["Read".to_string()]));
        assert_eq!(fm.version(), 4);
    }

    #[test]
    fn declared_tools_unions_directly_constructed_frontmatter() {
        // The accessor recomputes the union for a Frontmatter built directly
        // (not via `parse`, which would have folded already).
        let fm = Frontmatter {
            name: "x".into(),
            description: "y".into(),
            allowed_tools: Some(vec!["read_file".into()]),
            tools: Some(vec!["shell".into(), "read_file".into()]),
            version: None,
        };
        assert_eq!(
            fm.declared_tools(),
            Some(vec!["read_file".to_string(), "shell".to_string()])
        );
    }
}
