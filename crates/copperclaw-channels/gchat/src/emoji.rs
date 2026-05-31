//! Emoji shortcode → unicode codepoint mapping.
//!
//! Google Chat's `reactions` endpoint expects a literal unicode emoji
//! character. Our agents emit shortcodes (`thumbsup`, `heart`, etc.) — we
//! translate them to a `char` by way of the numeric codepoint.
//!
//! Hard project rule: **no literal emoji characters** anywhere in source.
//! This table only stores numeric codepoints (`0x1F44D` etc.) and uses
//! [`char::from_u32`] to materialize the character at call time. That way
//! the source tree stays emoji-free even though the API ultimately needs
//! unicode.

/// Lookup table mapping shortcode strings to their unicode codepoint.
///
/// Add new entries here when the agent needs additional reactions.
pub const EMOJI_TABLE: &[(&str, u32)] = &[
    ("thumbsup", 0x1F44D),
    ("thumbsdown", 0x1F44E),
    ("heart", 0x2764),
    ("tada", 0x1F389),
    ("eyes", 0x1F440),
    ("rocket", 0x1F680),
    ("fire", 0x1F525),
    ("smile", 0x1F600),
    ("cry", 0x1F622),
    ("clap", 0x1F44F),
    ("ok_hand", 0x1F44C),
    ("pray", 0x1F64F),
    ("wave", 0x1F44B),
    ("100", 0x1F4AF),
    ("warning", 0x26A0),
    ("check", 0x2705),
    ("x", 0x274C),
    ("question", 0x2753),
];

/// Look up a shortcode and return the matching `char`.
///
/// Unknown shortcodes return `None`; the adapter maps `None` to
/// [`copperclaw_channels_core::AdapterError::Unsupported`].
#[must_use]
pub fn emoji_codepoint(shortcode: &str) -> Option<char> {
    EMOJI_TABLE
        .iter()
        .find(|(name, _)| *name == shortcode)
        .and_then(|(_, cp)| char::from_u32(*cp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_non_empty() {
        // EMOJI_TABLE is a const slice; this exists to catch a future
        // mistake where someone empties the table.
        assert!(EMOJI_TABLE.len() >= 4, "table shrank unexpectedly");
    }

    #[test]
    fn every_table_entry_is_a_valid_char() {
        for (name, cp) in EMOJI_TABLE {
            assert!(
                char::from_u32(*cp).is_some(),
                "shortcode {name} maps to invalid codepoint {cp:#x}"
            );
        }
    }

    #[test]
    fn lookup_returns_some_for_each_table_entry() {
        for (name, cp) in EMOJI_TABLE {
            let got = emoji_codepoint(name).expect("known shortcode");
            assert_eq!(got as u32, *cp);
        }
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(emoji_codepoint("not-a-real-shortcode").is_none());
        assert!(emoji_codepoint("").is_none());
    }

    #[test]
    fn thumbsup_resolves_to_correct_codepoint() {
        let c = emoji_codepoint("thumbsup").unwrap();
        assert_eq!(c as u32, 0x1F44D);
    }

    #[test]
    fn heart_resolves_to_bmp_codepoint() {
        let c = emoji_codepoint("heart").unwrap();
        assert_eq!(c as u32, 0x2764);
    }

    #[test]
    fn table_has_no_duplicate_shortcodes() {
        let mut names: Vec<&str> = EMOJI_TABLE.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len, "duplicate shortcode in EMOJI_TABLE");
    }

    #[test]
    fn case_sensitive_lookup() {
        // The table uses lowercase; uppercase should not match.
        assert!(emoji_codepoint("thumbsup").is_some());
        assert!(emoji_codepoint("ThumbsUp").is_none());
    }
}
