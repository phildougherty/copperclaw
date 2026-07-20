//! Best-effort provider/model pricing table.
//!
//! Feeds the transcript status line and `cclaw usage` cost columns. Prices
//! are stored as **integer micro-dollars per megatoken** (1 USD == `1_000_000`
//! micros; 1 `MTok` == `1_000_000` tokens) — never floats — because values are
//! summed over millions of `agent_turns` rows and cross the host/runner
//! process boundary, where float drift and non-associativity would corrupt
//! rollups.
//!
//! The table is a *best-effort snapshot* of published list prices as of
//! [`PRICING_AS_OF`]. Vendors change prices; a later card surfaces
//! `PRICING_AS_OF` in `cclaw usage --json` so operators can see when the
//! numbers went stale. Introductory/promotional rates (e.g. Sonnet 5's
//! launch discount) are ignored in favor of the sticker price.
//!
//! Two deliberately distinct "no dollars" cases:
//!
//! - **Unknown model or unknown provider** -> `None`. A confidently wrong
//!   `$0.00` is worse than an honest blank, so surfaces render `—` for
//!   `None`, never zero.
//! - **Known-free local provider** (`ollama` — local inference, zero
//!   marginal API cost) -> `Some(ModelPrice { 0, 0 })`. The provider being
//!   recognized as free infrastructure is positive knowledge, not absence
//!   of knowledge, so it must not collapse into the `None` case.
//!
//! Subprocess providers (`codex`, `opencode`) bill through their own
//! vendor subscriptions with no per-token price visible to us, so they
//! return `None`.

/// Price of one megatoken of input and output, in micro-dollars.
///
/// `input_per_mtok_micros == 5_000_000` means $5.00 per million input
/// tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPrice {
    /// Micro-dollars per million input tokens.
    pub input_per_mtok_micros: u64,
    /// Micro-dollars per million output tokens.
    pub output_per_mtok_micros: u64,
}

/// Date (YYYY-MM-DD) the pricing table below was last verified against
/// published list prices. Surfaced to operators via `cclaw usage --json`
/// so stale numbers are self-evident.
pub const PRICING_AS_OF: &str = "2026-07-20";

const fn price(input: u64, output: u64) -> ModelPrice {
    ModelPrice {
        input_per_mtok_micros: input,
        output_per_mtok_micros: output,
    }
}

// Anthropic tier constants (micro-dollars per MTok).
const FABLE_5: ModelPrice = price(10_000_000, 50_000_000); // $10 / $50
const OPUS_CURRENT: ModelPrice = price(5_000_000, 25_000_000); // $5 / $25 (Opus 4.5+)
const OPUS_LEGACY: ModelPrice = price(15_000_000, 75_000_000); // $15 / $75 (Opus <= 4.1, Claude 3 Opus)
const SONNET: ModelPrice = price(3_000_000, 15_000_000); // $3 / $15 (all Sonnet tiers)
const HAIKU_4_5: ModelPrice = price(1_000_000, 5_000_000); // $1 / $5
const HAIKU_3_5: ModelPrice = price(800_000, 4_000_000); // $0.80 / $4
const HAIKU_3: ModelPrice = price(250_000, 1_250_000); // $0.25 / $1.25

/// Exact-match table: bare model ids / aliases as the API accepts them.
/// Checked before the prefix table.
const ANTHROPIC_EXACT: &[(&str, ModelPrice)] = &[
    ("claude-fable-5", FABLE_5),
    ("claude-mythos-5", FABLE_5),
    ("claude-opus-4-8", OPUS_CURRENT),
    ("claude-opus-4-7", OPUS_CURRENT),
    ("claude-opus-4-6", OPUS_CURRENT),
    ("claude-opus-4-5", OPUS_CURRENT),
    ("claude-opus-4-1", OPUS_LEGACY),
    ("claude-opus-4-0", OPUS_LEGACY),
    ("claude-sonnet-5", SONNET),
    ("claude-sonnet-4-6", SONNET),
    ("claude-sonnet-4-5", SONNET),
    ("claude-sonnet-4-0", SONNET),
    ("claude-haiku-4-5", HAIKU_4_5),
];

/// Prefix table for dated snapshot ids (`claude-sonnet-4-5-20250929`,
/// `claude-haiku-4-5-20251001`, ...). Ordered most-specific-first so a
/// longer family prefix wins before a shorter one could misprice it —
/// e.g. `claude-opus-4-5*` ($5/$25) must match before any legacy Opus 4
/// rule ($15/$75) could.
const ANTHROPIC_PREFIX: &[(&str, ModelPrice)] = &[
    ("claude-fable-5", FABLE_5),
    ("claude-mythos-5", FABLE_5),
    ("claude-opus-4-8", OPUS_CURRENT),
    ("claude-opus-4-7", OPUS_CURRENT),
    ("claude-opus-4-6", OPUS_CURRENT),
    ("claude-opus-4-5", OPUS_CURRENT),
    ("claude-opus-4-1", OPUS_LEGACY),
    ("claude-opus-4-0", OPUS_LEGACY),
    ("claude-opus-4-2025", OPUS_LEGACY), // dated Opus 4.0 (claude-opus-4-20250514)
    ("claude-sonnet-5", SONNET),
    ("claude-sonnet-4", SONNET), // 4-6 / 4-5 / 4-0 / dated — all $3/$15
    ("claude-haiku-4-5", HAIKU_4_5),
    // Claude 3.x family (version-before-family naming).
    ("claude-3-7-sonnet", SONNET),
    ("claude-3-5-sonnet", SONNET),
    ("claude-3-5-haiku", HAIKU_3_5),
    ("claude-3-haiku", HAIKU_3),
    ("claude-3-opus", OPUS_LEGACY),
];

/// Look up the list price for `model` on `provider`.
///
/// - Unknown provider or unknown model -> `None` (render `—`, never $0.00).
/// - `ollama` -> `Some(ModelPrice { 0, 0 })` for any model: local
///   inference is genuinely zero-cost infrastructure, and a *known-free
///   provider* is not an *unknown model*.
/// - `anthropic` -> exact-match table first, then prefix matching so
///   dated ids (`claude-sonnet-5-2025...`) resolve to their family.
///
/// Model matching mirrors `is_anthropic_family_model` in
/// `copperclaw-providers`: trim + ASCII-lowercase, take the final path
/// segment of vendor-prefixed slugs (`anthropic/claude-3.7-sonnet`), and
/// additionally normalize `.` to `-` so `OpenRouter`'s dotted versions hit
/// the dashed table entries.
#[must_use]
pub fn price_for(provider: &str, model: &str) -> Option<ModelPrice> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "ollama" => Some(price(0, 0)),
        "anthropic" => anthropic_price(model),
        _ => None,
    }
}

fn anthropic_price(model: &str) -> Option<ModelPrice> {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    // Final path segment of a vendor-prefixed slug, or the bare id.
    let leaf = m.rsplit('/').next().unwrap_or(&m);
    // OpenRouter spells versions with dots (`claude-3.7-sonnet`).
    let leaf = leaf.replace('.', "-");

    for (id, p) in ANTHROPIC_EXACT {
        if *id == leaf {
            return Some(*p);
        }
    }
    for (prefix, p) in ANTHROPIC_PREFIX {
        if leaf.starts_with(prefix) {
            return Some(*p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_hit() {
        assert_eq!(
            price_for("anthropic", "claude-opus-4-8"),
            Some(price(5_000_000, 25_000_000))
        );
        assert_eq!(
            price_for("anthropic", "claude-haiku-4-5"),
            Some(price(1_000_000, 5_000_000))
        );
    }

    #[test]
    fn prefix_match_hit_for_dated_ids() {
        // Dated snapshot ids resolve to their family price.
        assert_eq!(
            price_for("anthropic", "claude-sonnet-4-5-20250929"),
            Some(price(3_000_000, 15_000_000))
        );
        assert_eq!(
            price_for("anthropic", "claude-sonnet-5-20260601"),
            Some(price(3_000_000, 15_000_000))
        );
        assert_eq!(
            price_for("anthropic", "claude-haiku-4-5-20251001"),
            Some(price(1_000_000, 5_000_000))
        );
    }

    #[test]
    fn vendor_slug_and_dotted_version_match() {
        // OpenRouter-style slug: vendor prefix + dotted version.
        assert_eq!(
            price_for("anthropic", "anthropic/claude-3.7-sonnet"),
            Some(price(3_000_000, 15_000_000))
        );
        // Case-insensitive, whitespace-tolerant, like is_anthropic_family_model.
        assert_eq!(
            price_for("Anthropic", "  Claude-Opus-4-6  "),
            Some(price(5_000_000, 25_000_000))
        );
    }

    #[test]
    fn legacy_opus_tier_not_mispriced_by_prefix_order() {
        assert_eq!(
            price_for("anthropic", "claude-opus-4-1-20250805"),
            Some(price(15_000_000, 75_000_000))
        );
        assert_eq!(
            price_for("anthropic", "claude-opus-4-20250514"),
            Some(price(15_000_000, 75_000_000))
        );
        // The $5/$25 tier must not be swallowed by legacy rules.
        assert_eq!(
            price_for("anthropic", "claude-opus-4-5-20251101"),
            Some(price(5_000_000, 25_000_000))
        );
    }

    #[test]
    fn unknown_model_is_none_never_zero() {
        // Unknown model on a known paid provider: None, and specifically
        // NOT a zero price — a confidently wrong $0.00 is worse than an
        // honest blank.
        let p = price_for("anthropic", "claude-nova-9");
        assert_eq!(p, None);
        assert_ne!(p, Some(price(0, 0)));
        assert_eq!(price_for("anthropic", "gpt-4o"), None);
        assert_eq!(price_for("anthropic", ""), None);
    }

    #[test]
    fn unknown_provider_is_none() {
        assert_eq!(price_for("openai", "gpt-4o"), None);
        // Known providers with no per-token price visibility (vendor CLIs).
        assert_eq!(price_for("codex", "gpt-5-codex"), None);
        assert_eq!(price_for("opencode", "claude-opus-4-8"), None);
        assert_eq!(price_for("", "claude-opus-4-8"), None);
    }

    #[test]
    fn known_local_provider_is_zero_priced_not_unknown() {
        // ollama is recognized free local infra: Some(0,0), any model.
        assert_eq!(price_for("ollama", "qwen3.6:27b"), Some(price(0, 0)));
        assert_eq!(price_for("Ollama", "gemma4:31b"), Some(price(0, 0)));
        // Distinct from the unknown case, which is None.
        assert_ne!(price_for("ollama", "qwen3.6:27b"), None);
    }

    #[test]
    fn pricing_as_of_is_valid_yyyy_mm_dd() {
        let bytes = PRICING_AS_OF.as_bytes();
        assert_eq!(bytes.len(), 10, "must be exactly YYYY-MM-DD");
        for (i, b) in bytes.iter().enumerate() {
            if i == 4 || i == 7 {
                assert_eq!(*b, b'-', "separators at positions 4 and 7");
            } else {
                assert!(b.is_ascii_digit(), "digit at position {i}");
            }
        }
        let month: u32 = PRICING_AS_OF[5..7].parse().expect("month parses");
        let day: u32 = PRICING_AS_OF[8..10].parse().expect("day parses");
        assert!((1..=12).contains(&month), "month in 1..=12");
        assert!((1..=31).contains(&day), "day in 1..=31");
    }
}
