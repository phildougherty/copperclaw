//! Mapping from caller-friendly emoji shortcodes to GitHub reaction slugs.
//!
//! GitHub's reactions API accepts only a fixed set of 8 slugs:
//! `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket`, `eyes`.
//!
//! We accept a wider set of common aliases (`thumbsup`, `+1`, etc.) so the
//! outbound `{"action":"reaction","emoji":"thumbsup"}` shape callers already
//! use for other channels keeps working. Unrecognized inputs return `None`;
//! the adapter surfaces that as [`AdapterError::BadRequest`].
//!
//! [`AdapterError::BadRequest`]: ironclaw_channels_core::AdapterError::BadRequest

/// Map a caller-supplied emoji shortcode to a GitHub reaction slug.
///
/// Returns `None` if the shortcode is not one of the supported aliases.
#[must_use]
pub fn to_reaction_slug(input: &str) -> Option<&'static str> {
    match input {
        "+1" | "thumbsup" | "thumbs_up" => Some("+1"),
        "-1" | "thumbsdown" | "thumbs_down" => Some("-1"),
        "laugh" | "smile" => Some("laugh"),
        "confused" => Some("confused"),
        "heart" | "love" => Some("heart"),
        "hooray" | "tada" => Some("hooray"),
        "rocket" => Some("rocket"),
        "eyes" => Some("eyes"),
        _ => None,
    }
}

/// The full set of slugs the GitHub API accepts. Useful for documentation /
/// test assertions.
pub const VALID_REACTION_SLUGS: &[&str] = &[
    "+1", "-1", "laugh", "confused", "heart", "hooray", "rocket", "eyes",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbs_up_aliases_map_to_plus_one() {
        assert_eq!(to_reaction_slug("+1"), Some("+1"));
        assert_eq!(to_reaction_slug("thumbsup"), Some("+1"));
        assert_eq!(to_reaction_slug("thumbs_up"), Some("+1"));
    }

    #[test]
    fn thumbs_down_aliases_map_to_minus_one() {
        assert_eq!(to_reaction_slug("-1"), Some("-1"));
        assert_eq!(to_reaction_slug("thumbsdown"), Some("-1"));
        assert_eq!(to_reaction_slug("thumbs_down"), Some("-1"));
    }

    #[test]
    fn laugh_aliases() {
        assert_eq!(to_reaction_slug("laugh"), Some("laugh"));
        assert_eq!(to_reaction_slug("smile"), Some("laugh"));
    }

    #[test]
    fn confused_maps_to_itself() {
        assert_eq!(to_reaction_slug("confused"), Some("confused"));
    }

    #[test]
    fn heart_aliases() {
        assert_eq!(to_reaction_slug("heart"), Some("heart"));
        assert_eq!(to_reaction_slug("love"), Some("heart"));
    }

    #[test]
    fn hooray_aliases() {
        assert_eq!(to_reaction_slug("hooray"), Some("hooray"));
        assert_eq!(to_reaction_slug("tada"), Some("hooray"));
    }

    #[test]
    fn rocket_maps_to_itself() {
        assert_eq!(to_reaction_slug("rocket"), Some("rocket"));
    }

    #[test]
    fn eyes_maps_to_itself() {
        assert_eq!(to_reaction_slug("eyes"), Some("eyes"));
    }

    #[test]
    fn unknown_shortcode_returns_none() {
        assert!(to_reaction_slug("unknown").is_none());
        assert!(to_reaction_slug("").is_none());
        assert!(to_reaction_slug("smile-cat").is_none());
    }

    #[test]
    fn unicode_emoji_returns_none() {
        // We deliberately do not ship a unicode-to-slug map in v1.
        // (Source: bytes 0xF0 0x9F 0x91 0x8D — encoded via escape to avoid
        // having unicode emoji literals in source.)
        let thumbs_up_unicode = String::from_utf8(vec![0xF0, 0x9F, 0x91, 0x8D]).unwrap();
        assert!(to_reaction_slug(&thumbs_up_unicode).is_none());
    }

    #[test]
    fn case_sensitive_no_match() {
        assert!(to_reaction_slug("ThumbsUp").is_none());
        assert!(to_reaction_slug("HEART").is_none());
    }

    #[test]
    fn all_outputs_are_valid_github_slugs() {
        let inputs = [
            "+1",
            "thumbsup",
            "thumbs_up",
            "-1",
            "thumbsdown",
            "thumbs_down",
            "laugh",
            "smile",
            "confused",
            "heart",
            "love",
            "hooray",
            "tada",
            "rocket",
            "eyes",
        ];
        for input in inputs {
            let slug = to_reaction_slug(input).expect("known input");
            assert!(
                VALID_REACTION_SLUGS.contains(&slug),
                "{slug} not in valid set"
            );
        }
    }

    #[test]
    fn valid_slugs_constant_has_eight_entries() {
        assert_eq!(VALID_REACTION_SLUGS.len(), 8);
    }
}
