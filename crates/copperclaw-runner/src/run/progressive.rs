//! M18 R6: progressive final answers.
//!
//! Once the H1 Task HUD exists, "something is happening" is already
//! covered during a long build (the self-editing HUD chip ticks while
//! tools run). What the HUD does NOT relieve is the *final answer*
//! landing all at once — after a multi-minute wait the user still gets
//! a wall of text in a single message.
//!
//! This module reveals that final answer progressively, by growing the
//! delivered message through a bounded sequence of in-place edits
//! instead of one terminal emit. It reuses the exact machinery H1's
//! HUD rides — a first emit that returns a stable anchor, then
//! `edit_message` rows keyed to that anchor — realised here for the
//! real chat answer: the first chunk is a `send_message` whose
//! [`ToolEffectAck::Message { seq }`] is the anchor, and each later
//! chunk is an `edit_message` targeting that `seq`. There is NO token
//! streaming through the transport (explicitly rejected in the M18
//! plan): the model's answer is already complete when we reach here, so
//! "growth" is a paced reveal of the finished text, not a live stream.
//!
//! Gating (all must hold — otherwise the caller keeps today's single
//! terminal emit, byte-identical):
//!
//!  - **Rich adapter only.** The originating channel's adapter must
//!    support in-place edits (`capabilities::supports_message_edit`,
//!    surfaced via [`super::hud::TaskHud::answer_edit_capable`]). On a
//!    bare adapter every edit would degrade into a fresh message — the
//!    exact new-message spam the HUD design avoids — so bare channels
//!    keep the single emit.
//!  - **Long turn only.** The turn must already have run past
//!    [`MIN_ELAPSED`]. A quick turn's answer never made the user wait,
//!    so there is nothing to relieve.
//!  - **Long-but-not-huge answer.** The answer must be at least
//!    [`MIN_GROW_CHARS`] (short replies aren't worth the extra edit
//!    rows) and must NOT already qualify for the slice-3.4 long-output
//!    expander chip (`build_expander_decorator`) — pages-of-output
//!    answers keep their existing collapsible treatment, so the two
//!    surfaces never overlap.
//!
//! Default ON: unlike most M18 capabilities this needs no opt-in (the
//! card explicitly changes the default), and it stays entirely within
//! the runner — no config, no new channel-adapter surface.

use std::time::Duration;

use super::RunnerDeps;

// ---------------------------------------------------------------------
// Tuning constants (M22 R1).
//
// Two independent groups of knobs:
//
//  - *Eligibility* (`MIN_ELAPSED`, `MIN_GROW_CHARS`): which replies get
//    a progressive reveal at all. Widening these makes more replies
//    feel live but writes no extra edits per reveal.
//  - *Edit budget* (`STEP_CHARS`, `MAX_STEPS`, `STEP_INTERVAL`): how
//    many in-place edits one reveal may issue and how fast. This is
//    what platform edit rate limits see. Per reply the burst is at most
//    `MAX_STEPS - 1` edits, one per `STEP_INTERVAL` — with the current
//    values 7 edits over ~5.6s, the same order as the previous 5 edits
//    over ~4.0s, and the per-edit cadence (the rate-limit-relevant
//    figure; Telegram tolerates roughly one edit per second per chat
//    sustained) is unchanged at 800ms. `MAX_STEPS` only bites on
//    answers longer than `STEP_CHARS * (MAX_STEPS - 1)` chars, so
//    raising it smooths very long answers without touching typical
//    ones. Keep `STEP_INTERVAL >= 800ms` — do not lower it without the
//    live measurement below.
//
// The M22 R1 tuning widened eligibility only (elapsed 30s -> 15s,
// length 280 -> 200 chars, `MAX_STEPS` 6 -> 8) and deliberately did NOT
// increase edit frequency. PENDING: final values await verification
// against a real Telegram client per the M22 plan ("measure against a
// real Telegram client before settling values; do not tune blind").
// ---------------------------------------------------------------------

/// Eligibility: turns shorter than this keep today's single terminal
/// emit — a quick answer never kept the user waiting, so a paced reveal
/// would only *delay* it. Read off the HUD's existing `started_at`
/// clock via [`super::hud::TaskHud::elapsed`].
pub(super) const MIN_ELAPSED: Duration = Duration::from_secs(15);

/// Eligibility: final answers shorter than this (in `char`s) are
/// emitted in one shot even on a long, edit-capable turn — a couple of
/// short lines don't benefit from a progressive reveal and the extra
/// edit rows aren't worth it.
pub(super) const MIN_GROW_CHARS: usize = 200;

/// Edit budget: target number of `char`s revealed per step. Chosen so a
/// multi-paragraph answer animates in a handful of edits rather than
/// dozens.
const STEP_CHARS: usize = 240;

/// Edit budget: hard cap on reveal steps regardless of answer length,
/// so a very long answer can't spray the delivery loop with dozens of
/// edit rows. The final step always carries the full text.
const MAX_STEPS: usize = 8;

/// Edit budget: wall-clock pacing between reveal edits — the knob edit
/// rate limits actually see. Staggers the row writes so the host's ~1s
/// delivery poll picks them up across several ticks and the message
/// visibly grows, rather than all edits landing in one poll and
/// collapsing to "final text appears at once".
pub(super) const STEP_INTERVAL: Duration = Duration::from_millis(800);

/// Whether a (reasoning-stripped) final answer is worth revealing
/// progressively: long enough to bother, but not so long it already
/// earns the long-output expander chip. Pure; unit-tested.
pub(super) fn worth_growing(answer: &str) -> bool {
    answer.chars().count() >= MIN_GROW_CHARS
        && crate::tools::build_expander_decorator(answer).is_none()
}

/// The full R6 gate: rich adapter + long turn + growable answer. Pure so
/// every arm (bare adapter, sub-30s turn, short answer, huge answer) is
/// unit-testable without a clock or a channel. `answer` must already be
/// reasoning-stripped (what the user will actually see).
pub(super) fn should_grow(edit_capable: bool, elapsed: Duration, answer: &str) -> bool {
    edit_capable && elapsed >= MIN_ELAPSED && worth_growing(answer)
}

/// R6 metric helper: the first gate arm that declined growth, or `None` when
/// [`should_grow`] would return `true`. Mirrors `should_grow`'s ordering so the
/// `copperclaw_progressive_final_skipped_total{reason}` label names the exact
/// arm that fired.
pub(super) fn grow_skip_reason(
    edit_capable: bool,
    elapsed: Duration,
    answer: &str,
) -> Option<&'static str> {
    if !edit_capable {
        Some("bare_adapter")
    } else if elapsed < MIN_ELAPSED {
        Some("short_turn")
    } else if answer.chars().count() < MIN_GROW_CHARS {
        Some("short_answer")
    } else if crate::tools::build_expander_decorator(answer).is_some() {
        Some("expander_scale")
    } else {
        None
    }
}

/// Split `answer` into the ascending sequence of growing prefixes to
/// reveal. The first element is the initial post; each subsequent one is
/// a strictly-longer prefix; the last is always the complete `answer`.
/// Boundaries are on `char` counts so a multi-byte codepoint is never
/// split mid-sequence. Pure; unit-tested.
///
/// Only called on answers that passed [`worth_growing`], so
/// `answer.chars().count() >= MIN_GROW_CHARS` and the result has at
/// least two elements (i.e. at least one edit).
pub(super) fn reveal_steps(answer: &str) -> Vec<String> {
    let chars: Vec<char> = answer.chars().collect();
    let total = chars.len();
    // ceil(total / STEP_CHARS), clamped into [1, MAX_STEPS].
    let steps = total.div_ceil(STEP_CHARS.max(1)).clamp(1, MAX_STEPS);
    let mut out: Vec<String> = Vec::with_capacity(steps);
    for i in 1..=steps {
        // Even-ish boundaries; the final step (i == steps) lands exactly
        // on `total`.
        let upto = (total.saturating_mul(i) / steps).min(total);
        out.push(chars[..upto].iter().collect());
    }
    // Belt-and-braces: guarantee the terminal step is the whole answer
    // regardless of integer-division rounding.
    if let Some(last) = out.last_mut() {
        if last.chars().count() != total {
            answer.clone_into(last);
        }
    }
    out
}

/// Reveal `answer` by posting the first chunk as a `send_message` then
/// growing it with `edit_message` rows keyed to that message's `seq`,
/// pausing `step_interval` between edits. Best-effort after the first
/// emit: an edit-row write failure stops the reveal but never sinks the
/// turn (the answer's authoritative copy is already in history, and the
/// first chunk is delivered). The first emit propagates its error,
/// matching the pre-R6 single-`send_message` behaviour.
pub(super) async fn grow_final_answer(
    deps: &RunnerDeps,
    answer: String,
    step_interval: Duration,
) -> anyhow::Result<()> {
    let all_steps = reveal_steps(&answer);
    copperclaw_metrics::observe_progressive_final_steps(all_steps.len() as u64);
    let mut steps = all_steps.into_iter();
    // `reveal_steps` always yields >= 1 element for a non-empty answer.
    let Some(first) = steps.next() else {
        return Ok(());
    };
    let spec = copperclaw_mcp::SendMessageSpec {
        to: None,
        text: first,
    };
    let ack = deps
        .tool_ctx
        .emit_outbound(copperclaw_mcp::OutboundToolEffect::SendMessage(spec))
        .await
        .map_err(|e| anyhow::anyhow!("send_message failed: {e}"))?;
    let seq = match ack {
        copperclaw_mcp::ToolEffectAck::Message { seq } => seq,
        // `send_message` always returns a `Message` ack; if a future
        // change breaks that we've already delivered the first chunk and
        // have no anchor to edit — stop here rather than guess.
        other => {
            tracing::warn!(
                target: "copperclaw_runner",
                ack = ?other,
                "progressive final answer: send_message returned no seq; \
                 skipping edit-based growth"
            );
            return Ok(());
        }
    };
    for chunk in steps {
        tokio::time::sleep(step_interval).await;
        let spec = copperclaw_mcp::EditMessageSpec {
            message_seq: seq,
            text: chunk,
        };
        if let Err(e) = deps
            .tool_ctx
            .emit_outbound(copperclaw_mcp::OutboundToolEffect::EditMessage(spec))
            .await
        {
            tracing::warn!(
                target: "copperclaw_runner",
                error = %e,
                seq,
                "progressive final answer: edit emit failed; stopping reveal"
            );
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worth_growing_rejects_short_answers() {
        assert!(!worth_growing("too short"));
        assert!(!worth_growing(&"x".repeat(MIN_GROW_CHARS - 1)));
    }

    #[test]
    fn worth_growing_accepts_medium_long_prose() {
        let answer = "para. ".repeat(60); // ~360 chars, few lines
        assert!(answer.chars().count() >= MIN_GROW_CHARS);
        assert!(worth_growing(&answer));
    }

    #[test]
    fn worth_growing_rejects_expander_scale_output() {
        // A pages-of-output answer earns the slice-3.4 expander chip
        // (>30 lines) — it must keep that treatment, not be grown.
        let huge = "line\n".repeat(40);
        assert!(crate::tools::build_expander_decorator(&huge).is_some());
        assert!(!worth_growing(&huge));
    }

    #[test]
    fn should_grow_requires_all_of_capability_elapsed_length() {
        let good = "word ".repeat(80); // ~400 chars, growable
        // Happy path.
        assert!(should_grow(true, Duration::from_secs(45), &good));
        // Bare adapter: never grow.
        assert!(!should_grow(false, Duration::from_secs(45), &good));
        // Sub-threshold turn: never grow (byte-identical single emit).
        assert!(!should_grow(true, Duration::from_secs(10), &good));
        // Exactly at the threshold is enough.
        assert!(should_grow(true, MIN_ELAPSED, &good));
        // Long turn but short answer: single emit.
        assert!(!should_grow(true, Duration::from_secs(120), "brief"));
    }

    #[test]
    fn should_grow_elapsed_boundary_is_fifteen_seconds() {
        let good = "word ".repeat(80); // ~400 chars, growable
        assert_eq!(MIN_ELAPSED, Duration::from_secs(15));
        // One second under the gate: single emit.
        assert!(!should_grow(true, Duration::from_secs(14), &good));
        // Exactly at the gate: grow.
        assert!(should_grow(true, Duration::from_secs(15), &good));
        // One second over: grow.
        assert!(should_grow(true, Duration::from_secs(16), &good));
    }

    #[test]
    fn worth_growing_length_boundary_is_two_hundred_chars() {
        assert_eq!(MIN_GROW_CHARS, 200);
        // One char under the gate: single emit.
        assert!(!worth_growing(&"x".repeat(199)));
        // Exactly at the gate: grow.
        assert!(worth_growing(&"x".repeat(200)));
    }

    #[test]
    fn reveal_steps_are_growing_prefixes_ending_in_full_text() {
        let answer = "abcdefghij".repeat(60); // 600 chars
        let steps = reveal_steps(&answer);
        assert!(steps.len() >= 2, "a growable answer reveals in >= 2 steps");
        assert!(steps.len() <= MAX_STEPS);
        // Strictly increasing lengths.
        for pair in steps.windows(2) {
            assert!(
                pair[1].chars().count() > pair[0].chars().count(),
                "each step must reveal more than the last"
            );
        }
        // Every step is a genuine prefix of the answer.
        for s in &steps {
            assert!(answer.starts_with(s.as_str()), "steps must be prefixes");
        }
        // The terminal step is the complete answer.
        assert_eq!(steps.last().unwrap(), &answer);
    }

    #[test]
    fn reveal_steps_caps_step_count_for_very_long_answers() {
        let answer = "z".repeat(STEP_CHARS * (MAX_STEPS + 5));
        let steps = reveal_steps(&answer);
        assert_eq!(steps.len(), MAX_STEPS, "step count is hard-capped");
        assert_eq!(steps.last().unwrap(), &answer);
    }

    #[test]
    fn reveal_steps_respects_char_boundaries() {
        // Multi-byte codepoints must never be split mid-sequence; every
        // prefix must round-trip as valid UTF-8 (guaranteed by `String`)
        // and remain a prefix of the source.
        let answer = "héllo wörld ".repeat(40); // accented, ~480 chars
        let steps = reveal_steps(&answer);
        for s in &steps {
            assert!(answer.starts_with(s.as_str()));
        }
        assert_eq!(steps.last().unwrap(), &answer);
    }
}
