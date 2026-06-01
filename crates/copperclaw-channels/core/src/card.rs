//! Canonical portable-card schema shared by every channel adapter.
//!
//! Wave 1 of the cards rollout sets up the types and the trait-level
//! fallback. Adapters with native card support (Telegram inline keyboards,
//! Slack Block Kit, Discord embeds, etc.) will override
//! [`crate::ChannelAdapter::deliver_card`] in wave 2 to render the card
//! structurally; everyone else gets a clean text rendering for free via
//! [`Card::to_text_fallback`].
//!
//! Limits are intentionally conservative — for any numeric / length cap
//! we adopt the strictest of the major channels so a card validated here
//! will pass every native renderer downstream:
//!
//! - `value` payloads ≤ 64 bytes — Telegram `callback_data` ceiling.
//! - ≤ 25 fields — Discord embed-field ceiling.
//! - ≤ 8 buttons total — Telegram inline-keyboard rows-of-4 × 2.
//! - title ≤ 256, body ≤ 4000, field label ≤ 64, field value ≤ 1024,
//!   button label ≤ 64.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum bytes in a button `value` (the callback payload).
///
/// Standardised against Telegram's `callback_data` ceiling, the
/// strictest of the major platforms.
pub const MAX_BUTTON_VALUE_BYTES: usize = 64;
/// Maximum total buttons on a card. Telegram allows 4 buttons per row;
/// we limit to two rows so the card stays readable on phones.
pub const MAX_BUTTONS: usize = 8;
/// Maximum fields. Discord caps embed fields at 25.
pub const MAX_FIELDS: usize = 25;
/// Maximum characters in `Card::title`.
pub const MAX_TITLE_CHARS: usize = 256;
/// Maximum characters in `Card::body`.
pub const MAX_BODY_CHARS: usize = 4000;
/// Maximum characters in `CardField::label`.
pub const MAX_FIELD_LABEL_CHARS: usize = 64;
/// Maximum characters in `CardField::value`.
pub const MAX_FIELD_VALUE_CHARS: usize = 1024;
/// Maximum characters in `CardButton::label`.
pub const MAX_BUTTON_LABEL_CHARS: usize = 64;

/// Canonical, channel-agnostic card structure. The runner emits ONE of
/// these and every adapter renders it natively (or via the text fallback).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Card {
    /// Headline. Rendered as bold first line on text-only fallback,
    /// `*title*` in Slack mrkdwn, embed title on Discord, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Body paragraph. Markdown or plain text — adapters handle
    /// channel-specific escaping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Key-value field rows. Rendered as a small table / definition
    /// list in channels that support it; degraded to "Label: value"
    /// lines in the text fallback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<CardField>,
    /// Interactive buttons. The user's tap is delivered back to the
    /// agent as an inbound chat message containing the `value` (see
    /// the callback-routing design in wave 2). Buttons with a `url`
    /// open the URL instead of producing a callback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<CardButton>,
    /// Optional image. Rendered inline where supported (Slack image
    /// block, Discord embed thumbnail, Telegram photo attachment),
    /// linked-out otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
}

/// One labelled key/value row in a card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CardField {
    pub label: String,
    pub value: String,
    /// Hint: rendered inline (side-by-side with the next field) when
    /// the channel supports it. Defaults to false — full-width rows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inline: bool,
}

/// One interactive button on a card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CardButton {
    /// Label the user sees on the button.
    pub label: String,
    /// EITHER value (sent back as callback) OR url (opens link).
    /// Exactly one must be set — the validator enforces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Visual hint: "primary" | "danger" | "secondary" (default).
    /// Adapters that don't support styles ignore this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

/// Errors raised by [`Card::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardError {
    /// Card has no content (no title, body, fields, or image — buttons alone
    /// are not enough context for a user).
    Empty,
    /// `title` exceeded [`MAX_TITLE_CHARS`].
    TitleTooLong { len: usize, max: usize },
    /// `body` exceeded [`MAX_BODY_CHARS`].
    BodyTooLong { len: usize, max: usize },
    /// Field count exceeded [`MAX_FIELDS`].
    TooManyFields { count: usize, max: usize },
    /// Button count exceeded [`MAX_BUTTONS`].
    TooManyButtons { count: usize, max: usize },
    /// Field `label` exceeded [`MAX_FIELD_LABEL_CHARS`].
    FieldLabelTooLong {
        index: usize,
        len: usize,
        max: usize,
    },
    /// Field `value` exceeded [`MAX_FIELD_VALUE_CHARS`].
    FieldValueTooLong {
        index: usize,
        len: usize,
        max: usize,
    },
    /// Field `label` was empty.
    FieldLabelEmpty { index: usize },
    /// Button `label` was empty.
    ButtonLabelEmpty { index: usize },
    /// Button `label` exceeded [`MAX_BUTTON_LABEL_CHARS`].
    ButtonLabelTooLong {
        index: usize,
        len: usize,
        max: usize,
    },
    /// Button had neither `value` nor `url` set.
    ButtonMissingTarget { index: usize },
    /// Button had BOTH `value` and `url` set.
    ButtonBothValueAndUrl { index: usize },
    /// Button `value` exceeded [`MAX_BUTTON_VALUE_BYTES`].
    ButtonValueTooLong {
        index: usize,
        len: usize,
        max: usize,
    },
    /// Button `url` was not a syntactically valid http(s) URL.
    ButtonInvalidUrl { index: usize },
    /// `image_url` was not a syntactically valid http(s) URL.
    InvalidImageUrl,
}

impl fmt::Display for CardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(
                f,
                "card must have at least one of: title, body, fields, image_url"
            ),
            Self::TitleTooLong { len, max } => {
                write!(f, "title is {len} chars (max {max})")
            }
            Self::BodyTooLong { len, max } => {
                write!(f, "body is {len} chars (max {max})")
            }
            Self::TooManyFields { count, max } => {
                write!(f, "{count} fields exceeds maximum of {max}")
            }
            Self::TooManyButtons { count, max } => {
                write!(f, "{count} buttons exceeds maximum of {max}")
            }
            Self::FieldLabelTooLong { index, len, max } => {
                write!(f, "fields[{index}].label is {len} chars (max {max})")
            }
            Self::FieldValueTooLong { index, len, max } => {
                write!(f, "fields[{index}].value is {len} chars (max {max})")
            }
            Self::FieldLabelEmpty { index } => {
                write!(f, "fields[{index}].label must be non-empty")
            }
            Self::ButtonLabelEmpty { index } => {
                write!(f, "buttons[{index}].label must be non-empty")
            }
            Self::ButtonLabelTooLong { index, len, max } => {
                write!(f, "buttons[{index}].label is {len} chars (max {max})")
            }
            Self::ButtonMissingTarget { index } => write!(
                f,
                "buttons[{index}] must set exactly one of `value` or `url`"
            ),
            Self::ButtonBothValueAndUrl { index } => {
                write!(f, "buttons[{index}] set both `value` and `url`; pick one")
            }
            Self::ButtonValueTooLong { index, len, max } => {
                write!(f, "buttons[{index}].value is {len} bytes (max {max})")
            }
            Self::ButtonInvalidUrl { index } => write!(
                f,
                "buttons[{index}].url must be a syntactically valid http(s) URL"
            ),
            Self::InvalidImageUrl => {
                write!(f, "image_url must be a syntactically valid http(s) URL")
            }
        }
    }
}

impl std::error::Error for CardError {}

/// Minimal http/https URL sniff. We deliberately don't pull in a full URL
/// crate at this layer — adapters do their own platform-specific upload
/// validation. The check confirms the scheme + the presence of a host so
/// the model can't smuggle `javascript:` / `data:` payloads through.
fn is_http_url(s: &str) -> bool {
    let lower = s.trim();
    let rest = if let Some(r) = lower.strip_prefix("https://") {
        r
    } else if let Some(r) = lower.strip_prefix("http://") {
        r
    } else {
        return false;
    };
    // Require at least one character of host before any path/query.
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    !rest[..host_end].is_empty()
}

impl Card {
    /// Apply every schema rule. Returns the first violation encountered;
    /// callers may surface this directly to the model so it can fix the
    /// card and retry.
    pub fn validate(&self) -> Result<(), CardError> {
        // Non-empty: at least one of title / body / fields / image_url
        // must be present. A card with only buttons is suspicious —
        // buttons without context confuse the user.
        let has_title = self.title.as_deref().is_some_and(|t| !t.trim().is_empty());
        let has_body = self.body.as_deref().is_some_and(|t| !t.trim().is_empty());
        let has_fields = !self.fields.is_empty();
        let has_image = self
            .image_url
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());
        if !(has_title || has_body || has_fields || has_image) {
            return Err(CardError::Empty);
        }

        if let Some(t) = &self.title {
            let len = t.chars().count();
            if len > MAX_TITLE_CHARS {
                return Err(CardError::TitleTooLong {
                    len,
                    max: MAX_TITLE_CHARS,
                });
            }
        }
        if let Some(b) = &self.body {
            let len = b.chars().count();
            if len > MAX_BODY_CHARS {
                return Err(CardError::BodyTooLong {
                    len,
                    max: MAX_BODY_CHARS,
                });
            }
        }

        if self.fields.len() > MAX_FIELDS {
            return Err(CardError::TooManyFields {
                count: self.fields.len(),
                max: MAX_FIELDS,
            });
        }
        for (i, field) in self.fields.iter().enumerate() {
            if field.label.trim().is_empty() {
                return Err(CardError::FieldLabelEmpty { index: i });
            }
            let llen = field.label.chars().count();
            if llen > MAX_FIELD_LABEL_CHARS {
                return Err(CardError::FieldLabelTooLong {
                    index: i,
                    len: llen,
                    max: MAX_FIELD_LABEL_CHARS,
                });
            }
            let vlen = field.value.chars().count();
            if vlen > MAX_FIELD_VALUE_CHARS {
                return Err(CardError::FieldValueTooLong {
                    index: i,
                    len: vlen,
                    max: MAX_FIELD_VALUE_CHARS,
                });
            }
        }

        if self.buttons.len() > MAX_BUTTONS {
            return Err(CardError::TooManyButtons {
                count: self.buttons.len(),
                max: MAX_BUTTONS,
            });
        }
        for (i, btn) in self.buttons.iter().enumerate() {
            if btn.label.trim().is_empty() {
                return Err(CardError::ButtonLabelEmpty { index: i });
            }
            let llen = btn.label.chars().count();
            if llen > MAX_BUTTON_LABEL_CHARS {
                return Err(CardError::ButtonLabelTooLong {
                    index: i,
                    len: llen,
                    max: MAX_BUTTON_LABEL_CHARS,
                });
            }
            match (btn.value.as_deref(), btn.url.as_deref()) {
                (None, None) => return Err(CardError::ButtonMissingTarget { index: i }),
                (Some(_), Some(_)) => {
                    return Err(CardError::ButtonBothValueAndUrl { index: i });
                }
                (Some(v), None) => {
                    if v.len() > MAX_BUTTON_VALUE_BYTES {
                        return Err(CardError::ButtonValueTooLong {
                            index: i,
                            len: v.len(),
                            max: MAX_BUTTON_VALUE_BYTES,
                        });
                    }
                }
                (None, Some(u)) => {
                    if !is_http_url(u) {
                        return Err(CardError::ButtonInvalidUrl { index: i });
                    }
                }
            }
        }

        if let Some(img) = &self.image_url {
            if !is_http_url(img) {
                return Err(CardError::InvalidImageUrl);
            }
        }

        Ok(())
    }

    /// Plain-text rendering used by adapters without native card support.
    ///
    /// Aims for a layout that's scannable in any medium — email, SMS,
    /// CLI, IRC. Output is deterministic for a given input so replay
    /// fixtures can compare bytes.
    pub fn to_text_fallback(&self) -> String {
        let mut out = String::new();
        if let Some(t) = &self.title {
            let t = t.trim();
            if !t.is_empty() {
                out.push_str("**");
                out.push_str(t);
                out.push_str("**\n");
            }
        }
        if let Some(b) = &self.body {
            let b = b.trim();
            if !b.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(b);
                out.push('\n');
            }
        }
        if !self.fields.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            for f in &self.fields {
                out.push_str(&f.label);
                out.push_str(": ");
                out.push_str(&f.value);
                out.push('\n');
            }
        }
        if !self.buttons.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("Buttons:\n");
            for b in &self.buttons {
                out.push_str("- [");
                out.push_str(&b.label);
                out.push_str("] -> ");
                match (b.value.as_deref(), b.url.as_deref()) {
                    (Some(v), _) => {
                        out.push_str("callback:");
                        out.push_str(v);
                    }
                    (None, Some(u)) => out.push_str(u),
                    (None, None) => out.push_str("(no target)"),
                }
                out.push('\n');
            }
        }
        if let Some(img) = &self.image_url {
            let img = img.trim();
            if !img.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("[image: ");
                out.push_str(img);
                out.push_str("]\n");
            }
        }
        // Trim the trailing newline so callers can compose the output
        // into a larger message without a stray blank line.
        while out.ends_with('\n') {
            out.pop();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_card() -> Card {
        Card {
            title: Some("Order #42".into()),
            body: Some("Ready to confirm?".into()),
            fields: vec![
                CardField {
                    label: "Item".into(),
                    value: "Espresso".into(),
                    inline: true,
                },
                CardField {
                    label: "Price".into(),
                    value: "$4.50".into(),
                    inline: true,
                },
            ],
            buttons: vec![
                CardButton {
                    label: "Confirm".into(),
                    value: Some("confirm:42".into()),
                    url: None,
                    style: Some("primary".into()),
                },
                CardButton {
                    label: "Details".into(),
                    value: None,
                    url: Some("https://example.com/order/42".into()),
                    style: None,
                },
            ],
            image_url: Some("https://example.com/img.png".into()),
        }
    }

    #[test]
    fn validate_happy_path() {
        let c = populated_card();
        c.validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_card() {
        let c = Card::default();
        assert_eq!(c.validate().unwrap_err(), CardError::Empty);
    }

    #[test]
    fn validate_rejects_buttons_only_card() {
        let c = Card {
            buttons: vec![CardButton {
                label: "Click".into(),
                value: Some("x".into()),
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        assert_eq!(c.validate().unwrap_err(), CardError::Empty);
    }

    #[test]
    fn validate_rejects_title_too_long() {
        let c = Card {
            title: Some("a".repeat(MAX_TITLE_CHARS + 1)),
            ..Card::default()
        };
        assert!(matches!(c.validate(), Err(CardError::TitleTooLong { .. })));
    }

    #[test]
    fn validate_rejects_body_too_long() {
        let c = Card {
            body: Some("b".repeat(MAX_BODY_CHARS + 1)),
            ..Card::default()
        };
        assert!(matches!(c.validate(), Err(CardError::BodyTooLong { .. })));
    }

    #[test]
    fn validate_rejects_too_many_fields() {
        let c = Card {
            title: Some("hi".into()),
            fields: (0..=MAX_FIELDS)
                .map(|i| CardField {
                    label: format!("l{i}"),
                    value: "v".into(),
                    inline: false,
                })
                .collect(),
            ..Card::default()
        };
        assert!(matches!(c.validate(), Err(CardError::TooManyFields { .. })));
    }

    #[test]
    fn validate_rejects_too_many_buttons() {
        let c = Card {
            title: Some("hi".into()),
            buttons: (0..=MAX_BUTTONS)
                .map(|i| CardButton {
                    label: format!("b{i}"),
                    value: Some(format!("v{i}")),
                    url: None,
                    style: None,
                })
                .collect(),
            ..Card::default()
        };
        assert!(matches!(
            c.validate(),
            Err(CardError::TooManyButtons { .. })
        ));
    }

    #[test]
    fn validate_rejects_field_label_empty() {
        let c = Card {
            title: Some("hi".into()),
            fields: vec![CardField {
                label: "  ".into(),
                value: "v".into(),
                inline: false,
            }],
            ..Card::default()
        };
        assert!(matches!(
            c.validate(),
            Err(CardError::FieldLabelEmpty { index: 0 })
        ));
    }

    #[test]
    fn validate_rejects_field_label_too_long() {
        let c = Card {
            title: Some("hi".into()),
            fields: vec![CardField {
                label: "a".repeat(MAX_FIELD_LABEL_CHARS + 1),
                value: "v".into(),
                inline: false,
            }],
            ..Card::default()
        };
        assert!(matches!(
            c.validate(),
            Err(CardError::FieldLabelTooLong { index: 0, .. })
        ));
    }

    #[test]
    fn validate_rejects_field_value_too_long() {
        let c = Card {
            title: Some("hi".into()),
            fields: vec![CardField {
                label: "L".into(),
                value: "v".repeat(MAX_FIELD_VALUE_CHARS + 1),
                inline: false,
            }],
            ..Card::default()
        };
        assert!(matches!(
            c.validate(),
            Err(CardError::FieldValueTooLong { index: 0, .. })
        ));
    }

    #[test]
    fn validate_rejects_button_with_neither_value_nor_url() {
        let c = Card {
            title: Some("hi".into()),
            buttons: vec![CardButton {
                label: "Click".into(),
                value: None,
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        assert_eq!(
            c.validate().unwrap_err(),
            CardError::ButtonMissingTarget { index: 0 }
        );
    }

    #[test]
    fn validate_rejects_button_with_both_value_and_url() {
        let c = Card {
            title: Some("hi".into()),
            buttons: vec![CardButton {
                label: "Click".into(),
                value: Some("x".into()),
                url: Some("https://example.com".into()),
                style: None,
            }],
            ..Card::default()
        };
        assert_eq!(
            c.validate().unwrap_err(),
            CardError::ButtonBothValueAndUrl { index: 0 }
        );
    }

    #[test]
    fn validate_rejects_button_value_too_long() {
        let c = Card {
            title: Some("hi".into()),
            buttons: vec![CardButton {
                label: "Click".into(),
                value: Some("x".repeat(MAX_BUTTON_VALUE_BYTES + 1)),
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        assert!(matches!(
            c.validate(),
            Err(CardError::ButtonValueTooLong { index: 0, .. })
        ));
    }

    #[test]
    fn validate_rejects_button_label_empty() {
        let c = Card {
            title: Some("hi".into()),
            buttons: vec![CardButton {
                label: "  ".into(),
                value: Some("x".into()),
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        assert!(matches!(
            c.validate(),
            Err(CardError::ButtonLabelEmpty { index: 0 })
        ));
    }

    #[test]
    fn validate_rejects_button_label_too_long() {
        let c = Card {
            title: Some("hi".into()),
            buttons: vec![CardButton {
                label: "a".repeat(MAX_BUTTON_LABEL_CHARS + 1),
                value: Some("x".into()),
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        assert!(matches!(
            c.validate(),
            Err(CardError::ButtonLabelTooLong { index: 0, .. })
        ));
    }

    #[test]
    fn validate_rejects_button_url_bad_scheme() {
        let c = Card {
            title: Some("hi".into()),
            buttons: vec![CardButton {
                label: "Click".into(),
                value: None,
                url: Some("javascript:alert(1)".into()),
                style: None,
            }],
            ..Card::default()
        };
        assert_eq!(
            c.validate().unwrap_err(),
            CardError::ButtonInvalidUrl { index: 0 }
        );
    }

    #[test]
    fn validate_rejects_image_url_bad_scheme() {
        let c = Card {
            title: Some("hi".into()),
            image_url: Some("file:///etc/passwd".into()),
            ..Card::default()
        };
        assert_eq!(c.validate().unwrap_err(), CardError::InvalidImageUrl);
    }

    #[test]
    fn validate_accepts_http_image_url() {
        let c = Card {
            title: Some("hi".into()),
            image_url: Some("http://example.com/x.png".into()),
            ..Card::default()
        };
        c.validate().unwrap();
    }

    #[test]
    fn serde_roundtrip_full_card() {
        let original = populated_card();
        let json = serde_json::to_string(&original).unwrap();
        let back: Card = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn serde_skips_default_fields() {
        let c = Card {
            title: Some("hello".into()),
            ..Card::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        // Only `title` should appear; body / fields / buttons / image_url
        // are skipped via the serde attrs.
        assert_eq!(json, r#"{"title":"hello"}"#);
    }

    #[test]
    fn serde_skips_default_field_inline() {
        let f = CardField {
            label: "L".into(),
            value: "V".into(),
            inline: false,
        };
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, r#"{"label":"L","value":"V"}"#);
    }

    #[test]
    fn to_text_fallback_renders_full_card() {
        let out = populated_card().to_text_fallback();
        assert!(out.contains("**Order #42**"));
        assert!(out.contains("Ready to confirm?"));
        assert!(out.contains("Item: Espresso"));
        assert!(out.contains("Price: $4.50"));
        assert!(out.contains("Buttons:"));
        assert!(out.contains("[Confirm] -> callback:confirm:42"));
        assert!(out.contains("[Details] -> https://example.com/order/42"));
        assert!(out.contains("[image: https://example.com/img.png]"));
        assert!(!out.is_empty());
    }

    #[test]
    fn to_text_fallback_is_deterministic() {
        let a = populated_card().to_text_fallback();
        let b = populated_card().to_text_fallback();
        assert_eq!(a, b);
    }

    #[test]
    fn to_text_fallback_omits_missing_sections() {
        let c = Card {
            title: Some("Only title".into()),
            ..Card::default()
        };
        let out = c.to_text_fallback();
        assert_eq!(out, "**Only title**");
    }

    #[test]
    fn to_text_fallback_no_trailing_newline() {
        let out = populated_card().to_text_fallback();
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn card_error_display_messages() {
        // Sanity-check the Display impl produces non-empty distinct strings
        // for each variant so operator-facing logs are useful.
        let cases = [
            CardError::Empty,
            CardError::TitleTooLong { len: 1, max: 0 },
            CardError::BodyTooLong { len: 1, max: 0 },
            CardError::TooManyFields { count: 1, max: 0 },
            CardError::TooManyButtons { count: 1, max: 0 },
            CardError::FieldLabelTooLong {
                index: 0,
                len: 1,
                max: 0,
            },
            CardError::FieldValueTooLong {
                index: 0,
                len: 1,
                max: 0,
            },
            CardError::FieldLabelEmpty { index: 0 },
            CardError::ButtonLabelEmpty { index: 0 },
            CardError::ButtonLabelTooLong {
                index: 0,
                len: 1,
                max: 0,
            },
            CardError::ButtonMissingTarget { index: 0 },
            CardError::ButtonBothValueAndUrl { index: 0 },
            CardError::ButtonValueTooLong {
                index: 0,
                len: 1,
                max: 0,
            },
            CardError::ButtonInvalidUrl { index: 0 },
            CardError::InvalidImageUrl,
        ];
        for err in cases {
            let s = format!("{err}");
            assert!(!s.is_empty(), "empty display for {err:?}");
        }
    }

    #[test]
    fn is_http_url_accepts_http_and_https() {
        assert!(is_http_url("http://example.com"));
        assert!(is_http_url("https://example.com/path?q=1"));
        assert!(is_http_url("https://x.y/z"));
    }

    #[test]
    fn is_http_url_rejects_other_schemes_and_garbage() {
        assert!(!is_http_url(""));
        assert!(!is_http_url("ftp://example.com"));
        assert!(!is_http_url("javascript:alert(1)"));
        assert!(!is_http_url("data:text/html,hi"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url("https:///path"));
    }
}
