//! F2: surface an actionable "I'm blocked" wall to the user.
//!
//! Tool errors and policy/provenance/verify-gate/egress denials render
//! into the model's history as `Tool { is_error: true }` — the user
//! never sees them. When the model then loops or gives up, the HUD just
//! stops, indistinguishable from a hang. This module watches the tail of
//! a turn's tool results: when a turn ends WITHOUT a user-facing reply
//! and its tail is a *run* of denials on the SAME blocker (not a single
//! recovered error), the terminal-failure path swaps its generic apology
//! for ONE curated, per-blocker [`copperclaw_channels_core::ErrorCard`]
//! that says WHAT is blocked and the ACTIONABLE next step.
//!
//! The card text is *curated per category* — it never echoes the raw
//! tool-error string, so no internal detail (or injected content that
//! rode in on a tool result) leaks to the user. Classification keys off
//! the stable, already-good hint substrings the denial sites emit
//! (`self_mod::egress_allow_hint`, the verify-gate refusal in
//! `todo.rs`, the `policy.rs` deny reasons).

use copperclaw_channels_core::{ErrorCard, ErrorCardKind};

/// Minimum consecutive same-category denials at the tail of a turn
/// before a wall card fires. A single recovered error never surfaces
/// one: either the model worked around it and produced a normal answer
/// (the turn ends `Done`, which never consults the tail run at all), or
/// the run is length 1 and stays below this floor.
pub(super) const BLOCKER_RUN_THRESHOLD: usize = 2;

/// A category of hard blocker the agent can wall on. Each maps to one
/// curated user-facing card; the raw denial text is never surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockerCategory {
    /// Container egress is denied by default and a package registry
    /// (pip/npm/…) was unreachable (`self_mod::egress_allow_hint`).
    Egress,
    /// A todo could not be completed because its changes are unverified
    /// and there's no recorded `.copperclaw/verify` command (the
    /// verify-gate refusal in `todo.rs`).
    VerifyGate,
    /// A credentialed external action was blocked because the turn's
    /// context is tainted by untrusted-provenance content, so fresh
    /// approval is required (`policy.rs` provenance deny).
    Provenance,
    /// A credentialed external action was blocked because this is an
    /// autonomous (heartbeat/scheduled) turn with no human to approve
    /// it (`policy.rs` autonomous deny).
    Autonomous,
    /// A tool was refused by the group's tool profile, the sender's
    /// role, or the active skill's allowed-tools list (`policy.rs`
    /// role/profile/skill denies).
    Policy,
}

impl BlockerCategory {
    /// Stable label for the M1 metrics rider (wall cards by category).
    pub(super) fn metric_label(self) -> &'static str {
        match self {
            Self::Egress => "egress",
            Self::VerifyGate => "verify_gate",
            Self::Provenance => "provenance",
            Self::Autonomous => "autonomous",
            Self::Policy => "policy",
        }
    }

    /// The curated, injection-safe card for this blocker. Reuses the
    /// remediation the denial sites already teach the model, phrased for
    /// the human on the other end. No raw tool text, no `details` block.
    pub(super) fn to_error_card(self) -> ErrorCard {
        let (title, summary) = match self {
            Self::Egress => (
                "Blocked: can't reach a package registry",
                "I couldn't finish because the container can't reach the package \
                 registry it needs — outbound network is denied by default. Ask an \
                 operator to allow the registry for this agent (`cclaw groups config \
                 set-egress-allow <agent-group-id> --allow <host:port>`), then send \
                 the request again.",
            ),
            Self::VerifyGate => (
                "Blocked: changes need a verify step",
                "I couldn't finish because a step has changes I can't prove work, and \
                 there's no check recorded for this project. Add a one-line command to \
                 `.copperclaw/verify` (for example `npm test`), then ask me to try \
                 again.",
            ),
            Self::Provenance => (
                "Blocked: this needs your approval",
                "I couldn't finish because this turn read untrusted content (a fetched \
                 web page or an untrusted memory entry), so an action that reaches \
                 outside the sandbox needs fresh approval. Approve it, or send the \
                 request again in a new message and I'll retry cleanly.",
            ),
            Self::Autonomous => (
                "Blocked: this needs a person",
                "I couldn't take an action that reaches outside the sandbox on an \
                 automatic (scheduled) run — no one was here to approve it. Send the \
                 request yourself and I'll do it.",
            ),
            Self::Policy => (
                "Blocked: that action isn't permitted here",
                "I couldn't finish because an action I needed isn't allowed by this \
                 agent's tool profile, your role, or the skill that's loaded. An \
                 operator can widen the tool profile if it should be permitted.",
            ),
        };
        ErrorCard::new(ErrorCardKind::Internal, summary).with_title(title)
    }
}

/// Classify one *errored* tool result's content into a blocker
/// category, keyed off the stable hint substrings the denial sites
/// emit. Returns `None` for ordinary tool failures we have no curated
/// wall for (a shell non-zero exit, a missing file, a timeout) — those
/// stay model-only and fall through to the generic apology. Only ever
/// called on `is_error` results, so the substring collisions that could
/// bite a normal tool output don't apply.
pub(super) fn classify(content: &str) -> Option<BlockerCategory> {
    // Order matters: the autonomous / provenance provenance-gate denies
    // carry distinct phrases, checked before the generic profile deny.
    if content.contains("set-egress-allow") {
        Some(BlockerCategory::Egress)
    } else if content.contains("unverified changes since") {
        Some(BlockerCategory::VerifyGate)
    } else if content.contains("untrusted-provenance") {
        Some(BlockerCategory::Provenance)
    } else if content.contains("autonomous (heartbeat/scheduled) turn") {
        Some(BlockerCategory::Autonomous)
    } else if content.contains("tool profile.")
        || content.contains("(read-only).")
        || content.contains("active skill's allowed-tools")
    {
        Some(BlockerCategory::Policy)
    } else {
        None
    }
}

/// Rolling tail-run tracker over a turn's tool results. Observes each
/// executed call in original order; the tail run is the count of
/// consecutive errors of one blocker category ending at the last
/// observation. A success, a differently-categorised error, or an
/// unclassified error resets the run. A user-facing reply mid-turn
/// (a successful `send_message`) latches suppression: the wall never
/// fires once the user has already heard from the agent.
#[derive(Debug, Default)]
pub(super) struct BlockerRun {
    category: Option<BlockerCategory>,
    count: usize,
    /// Set once the model has sent the user a message this turn — after
    /// that no wall card fires, because the turn is not a silent hang.
    user_replied: bool,
}

impl BlockerRun {
    /// Observe one executed tool call's outcome in call order.
    /// `tool_name` distinguishes the user-facing `send_message` reply;
    /// `is_error` / `content` drive blocker classification.
    pub(super) fn observe(&mut self, tool_name: &str, is_error: bool, content: &str) {
        if tool_name == "send_message" && !is_error {
            // The agent talked to the user — this turn is not a silent
            // wall even if it later fails, so latch suppression.
            self.user_replied = true;
            self.category = None;
            self.count = 0;
            return;
        }
        match is_error.then(|| classify(content)).flatten() {
            Some(cat) if self.category == Some(cat) => self.count += 1,
            Some(cat) => {
                self.category = Some(cat);
                self.count = 1;
            }
            None => {
                self.category = None;
                self.count = 0;
            }
        }
    }

    /// The tail blocker to surface, or `None` when the run hasn't
    /// reached [`BLOCKER_RUN_THRESHOLD`] or the user already got a reply.
    pub(super) fn tail_blocker(&self) -> Option<BlockerCategory> {
        if self.user_replied || self.count < BLOCKER_RUN_THRESHOLD {
            return None;
        }
        self.category
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_each_stable_hint() {
        // The exact strings the denial sites emit (see self_mod.rs,
        // todo.rs, policy.rs). Guards against the hint wording drifting
        // out from under the classifier.
        assert_eq!(
            classify(
                "Tool `install_packages` failed: session install failed. `npm` could \
                 not reach its package registry — container egress is denied by \
                 default. Ask an operator to allow the registry, then retry: `cclaw \
                 groups config set-egress-allow abc --allow registry.npmjs.org:443`"
            ),
            Some(BlockerCategory::Egress)
        );
        assert_eq!(
            classify(
                "Tool `todo_update` failed: cannot mark todo 2 completed: `/data/app` \
                 has unverified changes since its last edit (no verify command \
                 recorded — write one to `.copperclaw/verify`)."
            ),
            Some(BlockerCategory::VerifyGate)
        );
        assert_eq!(
            classify(
                "Tool `web_fetch` takes a credentialed external action, but this turn's \
                 context contains untrusted-provenance content. Fresh approval is \
                 required."
            ),
            Some(BlockerCategory::Provenance)
        );
        assert_eq!(
            classify(
                "Tool `install_packages` takes a credentialed external action, which is \
                 not permitted on an autonomous (heartbeat/scheduled) turn."
            ),
            Some(BlockerCategory::Autonomous)
        );
        assert_eq!(
            classify("Tool `shell` is not permitted by the `messaging` tool profile."),
            Some(BlockerCategory::Policy)
        );
        assert_eq!(
            classify("Tool `edit_file` is not available to `guest` senders (read-only)."),
            Some(BlockerCategory::Policy)
        );
        assert_eq!(
            classify("Tool `shell` is not in the active skill's allowed-tools list."),
            Some(BlockerCategory::Policy)
        );
    }

    #[test]
    fn classify_ignores_ordinary_failures() {
        assert_eq!(
            classify("Tool `shell` failed: exit code 1: file not found"),
            None
        );
        assert_eq!(
            classify("Unknown tool `frobnicate` — no handler registered."),
            None
        );
        assert_eq!(classify(""), None);
    }

    #[test]
    fn run_fires_only_at_threshold() {
        let mut run = BlockerRun::default();
        run.observe("install_packages", true, "… set-egress-allow …");
        // One error is a candidate recovery, not a wall.
        assert_eq!(run.tail_blocker(), None, "single error must not fire");
        run.observe("install_packages", true, "… set-egress-allow …");
        assert_eq!(
            run.tail_blocker(),
            Some(BlockerCategory::Egress),
            "a run of two same-category denials fires the wall"
        );
    }

    #[test]
    fn success_between_denials_breaks_the_run() {
        let mut run = BlockerRun::default();
        run.observe("install_packages", true, "… set-egress-allow …");
        run.observe("read_file", false, "file contents");
        run.observe("install_packages", true, "… set-egress-allow …");
        assert_eq!(
            run.tail_blocker(),
            None,
            "an intervening success resets the tail run"
        );
    }

    #[test]
    fn different_category_resets_the_run() {
        let mut run = BlockerRun::default();
        run.observe("install_packages", true, "… set-egress-allow …");
        run.observe(
            "shell",
            true,
            "not permitted by the `messaging` tool profile.",
        );
        // The tail is now a length-1 Policy run.
        assert_eq!(run.tail_blocker(), None);
        run.observe(
            "shell",
            true,
            "not permitted by the `messaging` tool profile.",
        );
        assert_eq!(run.tail_blocker(), Some(BlockerCategory::Policy));
    }

    #[test]
    fn unclassified_error_breaks_the_run() {
        let mut run = BlockerRun::default();
        run.observe("install_packages", true, "… set-egress-allow …");
        run.observe("shell", true, "exit code 1");
        run.observe("install_packages", true, "… set-egress-allow …");
        assert_eq!(
            run.tail_blocker(),
            None,
            "an unclassified error resets the tail run just like a success"
        );
    }

    #[test]
    fn user_reply_latches_suppression() {
        let mut run = BlockerRun::default();
        run.observe("send_message", false, "here's an update");
        run.observe("install_packages", true, "… set-egress-allow …");
        run.observe("install_packages", true, "… set-egress-allow …");
        assert_eq!(
            run.tail_blocker(),
            None,
            "once the user got a reply the turn is not a silent wall"
        );
    }

    #[test]
    fn every_category_builds_a_valid_card() {
        for cat in [
            BlockerCategory::Egress,
            BlockerCategory::VerifyGate,
            BlockerCategory::Provenance,
            BlockerCategory::Autonomous,
            BlockerCategory::Policy,
        ] {
            let card = cat.to_error_card();
            card.validate()
                .unwrap_or_else(|e| panic!("{} card invalid: {e}", cat.metric_label()));
            assert!(
                card.details.is_none(),
                "wall card must not carry raw details"
            );
        }
    }
}
