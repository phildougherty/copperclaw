//! Skill-name validation.
//!
//! A valid skill name is kebab-case, starts with `[a-z0-9]`, contains only
//! `[a-z0-9-]`, and is at most 64 characters long. This matches the slug rule
//! `[a-z0-9][a-z0-9-]{0,63}`.

use crate::error::SkillError;

pub(crate) const MAX_NAME_LEN: usize = 64;

/// Validate that `name` matches `[a-z0-9][a-z0-9-]{0,63}`. Returns the name
/// trimmed of trailing newlines on success.
///
/// # Errors
/// - [`SkillError::InvalidName`] if `name` is empty, too long, starts with a
///   dash, or contains characters outside `[a-z0-9-]`.
pub fn validate(name: &str) -> Result<(), SkillError> {
    if name.is_empty() {
        return Err(SkillError::InvalidName("name is empty".to_string()));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(SkillError::InvalidName(format!(
            "name exceeds {MAX_NAME_LEN} characters: {name}"
        )));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !is_alnum_lower(first) {
        return Err(SkillError::InvalidName(format!(
            "name must start with [a-z0-9]: {name}"
        )));
    }
    for c in chars {
        if !(is_alnum_lower(c) || c == '-') {
            return Err(SkillError::InvalidName(format!(
                "name contains invalid character {c:?}: {name}"
            )));
        }
    }
    Ok(())
}

fn is_alnum_lower(c: char) -> bool {
    c.is_ascii_digit() || c.is_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_simple_name() {
        validate("foo").unwrap();
    }

    #[test]
    fn accepts_kebab() {
        validate("my-cool-skill-1").unwrap();
    }

    #[test]
    fn accepts_starting_digit() {
        validate("0abc").unwrap();
    }

    #[test]
    fn accepts_single_char() {
        validate("a").unwrap();
        validate("0").unwrap();
    }

    #[test]
    fn rejects_empty() {
        let err = validate("").unwrap_err();
        assert!(matches!(err, SkillError::InvalidName(_)));
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn rejects_uppercase() {
        let err = validate("Foo").unwrap_err();
        assert!(err.to_string().contains("start with"));
        let err2 = validate("a-B").unwrap_err();
        assert!(err2.to_string().contains("invalid character"));
    }

    #[test]
    fn rejects_leading_dash() {
        let err = validate("-foo").unwrap_err();
        assert!(err.to_string().contains("start with"));
    }

    #[test]
    fn rejects_underscore() {
        let err = validate("foo_bar").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn rejects_too_long() {
        let n = "a".repeat(MAX_NAME_LEN + 1);
        let err = validate(&n).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn accepts_max_length() {
        let n = "a".repeat(MAX_NAME_LEN);
        validate(&n).unwrap();
    }
}
