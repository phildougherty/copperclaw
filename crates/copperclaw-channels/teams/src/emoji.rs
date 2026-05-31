//! Mapping from emoji shortcodes to Microsoft Teams `reactionType` values.
//!
//! Microsoft Graph's `setReaction` endpoint only accepts six fixed values:
//! `like`, `heart`, `laugh`, `surprised`, `sad`, `angry`. Other emoji shortcodes
//! return [`None`]; callers should translate that into
//! [`copperclaw_channels_core::AdapterError::Unsupported`].

/// The six reaction types Microsoft Teams accepts on a chat message.
pub const TEAMS_REACTION_TYPES: &[&str] = &[
    "like", "heart", "laugh", "surprised", "sad", "angry",
];

/// Translate a common emoji shortcode into the `reactionType` Teams expects.
///
/// Returns [`None`] if the shortcode is not one Teams natively supports.
#[must_use]
pub fn shortcode_to_reaction_type(shortcode: &str) -> Option<&'static str> {
    // Strip surrounding colons if the caller passed a literal `:thumbsup:` style
    // shortcode, since some agents emit either form.
    let trimmed = shortcode.trim().trim_matches(':');
    match trimmed {
        // `like` plus common thumbs-up aliases.
        "like" | "thumbsup" | "+1" | "thumbs_up" => Some("like"),
        // `heart` plus common love aliases.
        "heart" | "heart_eyes" | "red_heart" | "love" => Some("heart"),
        // `laugh` plus common laugh aliases.
        "laugh" | "joy" | "laughing" | "rofl" | "smile" | "haha" => Some("laugh"),
        // `surprised` plus common wow aliases.
        "surprised" | "open_mouth" | "astonished" | "wow" => Some("surprised"),
        // `sad` plus common cry aliases.
        "sad" | "cry" | "frown" | "frowning" => Some("sad"),
        // `angry` plus common anger aliases.
        "angry" | "rage" | "angry_face" | "anger" => Some("angry"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_direct_names() {
        for name in TEAMS_REACTION_TYPES {
            assert_eq!(shortcode_to_reaction_type(name), Some(*name));
        }
    }

    #[test]
    fn maps_thumbsup_aliases_to_like() {
        assert_eq!(shortcode_to_reaction_type("thumbsup"), Some("like"));
        assert_eq!(shortcode_to_reaction_type("+1"), Some("like"));
        assert_eq!(shortcode_to_reaction_type("thumbs_up"), Some("like"));
    }

    #[test]
    fn maps_heart_aliases() {
        assert_eq!(shortcode_to_reaction_type("heart_eyes"), Some("heart"));
        assert_eq!(shortcode_to_reaction_type("red_heart"), Some("heart"));
        assert_eq!(shortcode_to_reaction_type("love"), Some("heart"));
    }

    #[test]
    fn maps_laugh_aliases() {
        assert_eq!(shortcode_to_reaction_type("joy"), Some("laugh"));
        assert_eq!(shortcode_to_reaction_type("laughing"), Some("laugh"));
        assert_eq!(shortcode_to_reaction_type("rofl"), Some("laugh"));
        assert_eq!(shortcode_to_reaction_type("smile"), Some("laugh"));
        assert_eq!(shortcode_to_reaction_type("haha"), Some("laugh"));
    }

    #[test]
    fn maps_surprised_aliases() {
        assert_eq!(shortcode_to_reaction_type("open_mouth"), Some("surprised"));
        assert_eq!(shortcode_to_reaction_type("astonished"), Some("surprised"));
        assert_eq!(shortcode_to_reaction_type("wow"), Some("surprised"));
    }

    #[test]
    fn maps_sad_aliases() {
        assert_eq!(shortcode_to_reaction_type("cry"), Some("sad"));
        assert_eq!(shortcode_to_reaction_type("frown"), Some("sad"));
        assert_eq!(shortcode_to_reaction_type("frowning"), Some("sad"));
    }

    #[test]
    fn maps_angry_aliases() {
        assert_eq!(shortcode_to_reaction_type("rage"), Some("angry"));
        assert_eq!(shortcode_to_reaction_type("angry_face"), Some("angry"));
        assert_eq!(shortcode_to_reaction_type("anger"), Some("angry"));
    }

    #[test]
    fn unknown_returns_none() {
        assert!(shortcode_to_reaction_type("unicorn").is_none());
        assert!(shortcode_to_reaction_type("").is_none());
        assert!(shortcode_to_reaction_type("xyzzy").is_none());
    }

    #[test]
    fn strips_colon_delimiters() {
        assert_eq!(shortcode_to_reaction_type(":thumbsup:"), Some("like"));
        assert_eq!(shortcode_to_reaction_type(":heart:"), Some("heart"));
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(shortcode_to_reaction_type("  like  "), Some("like"));
    }

    #[test]
    fn reaction_types_constant_has_six_values() {
        assert_eq!(TEAMS_REACTION_TYPES.len(), 6);
    }
}
