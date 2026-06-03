//! Tool authorization policy (PLAN.md § 6 T5, security-hardening Phase 1.1).
//!
//! Generalizes the old static [`DISALLOWED_TOOLS`] floor into a layered
//! gate evaluated at every tool dispatch (see
//! [`crate::run::tool_dispatch::invoke_tool`]). A tool call is permitted
//! only when it survives, in order:
//!
//! 1. **Host-owned floor.** [`DISALLOWED_TOOLS`] names are *never*
//!    reachable from inside the container, regardless of profile, role,
//!    or skill. This is the same case-sensitive list the runner has
//!    always enforced — it sits *beneath* the positive allow-list so no
//!    looser layer can re-grant a host-owned tool.
//! 2. **Sender role.** A [`SenderRole::Guest`] sender is held to a
//!    read-only floor: shell / file-mutation / self-modification tools
//!    are denied even if the active profile would otherwise allow them.
//! 3. **Active skill `allowed-tools`.** When the turn is running under a
//!    skill that declared an `allowed-tools` frontmatter list, only the
//!    named tools (plus the always-available housekeeping set) pass —
//!    so a skill declaring `allowed-tools: [Read]` blocks `shell`.
//! 4. **Group tool-profile.** The positive allow-list for the group:
//!    [`ToolProfile::Minimal`] / [`Messaging`] / [`Coding`] / [`Full`].
//!
//! Layers 2-4 are a positive allow-list (default-deny intersection);
//! layer 1 is a hard floor (default-allow with a deny-set carve-out).
//!
//! [`Messaging`]: ToolProfile::Messaging
//! [`Coding`]: ToolProfile::Coding
//! [`Full`]: ToolProfile::Full

use serde::{Deserialize, Serialize};

/// Built-in tool names the runner must always refuse, regardless of the
/// active profile, sender role, or skill. These are owned by the host
/// orchestrator (it implements them out-of-band) and must never be
/// reachable from inside the container.
///
/// Matching is case-sensitive on purpose: lower-case MCP variants (e.g.
/// our `ask_user_question`, owned by `copperclaw-mcp`) are *allowed* and
/// must not collide with the historical pascal-case built-in names here.
pub const DISALLOWED_TOOLS: &[&str] = &[
    "CronCreate",
    "CronDelete",
    "CronList",
    "ScheduleWakeup",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "EnterWorktree",
    "ExitWorktree",
];

/// Returns `true` if `name` exactly matches a host-owned disallowed tool.
/// Matching is case-sensitive (see [`DISALLOWED_TOOLS`]).
#[must_use]
pub fn is_disallowed(name: &str) -> bool {
    DISALLOWED_TOOLS.iter().any(|t| *t == name)
}

/// Tools that are available under *every* profile and to *every* role:
/// the conversation + session-housekeeping primitives. Blocking these
/// would leave even a minimal agent unable to reply or manage its own
/// context, so they sit at the base of every allow-list.
const ALWAYS_TOOLS: &[&str] = &[
    "send_message",
    "send_file",
    "edit_message",
    "add_reaction",
    "send_card",
    "ask_user_question",
    "load_skill",
    "todo_add",
    "todo_list",
    "todo_update",
    "todo_delete",
    "compact_now",
    "clear_history",
    "artifact_path",
];

/// Read-only / informational tools layered on top of [`ALWAYS_TOOLS`] by
/// the `messaging` profile (and inherited by richer profiles). Safe for a
/// guest sender: they observe but never mutate the filesystem or shell.
const READONLY_TOOLS: &[&str] = &[
    "read_file",
    "view_image",
    "glob",
    "grep",
    "git_blame",
    "git_diff",
    "git_log",
    "git_status",
    "web_search",
    "web_fetch",
    "list_tasks",
    "schedule_task",
    "cancel_task",
    "pause_task",
    "resume_task",
    "update_task",
];

/// Filesystem-mutation, shell, and agent-spawning tools layered on by the
/// `coding` profile. These are denied to a guest sender even when the
/// profile would allow them (see [`SenderRole::denies_mutating`]).
const CODING_TOOLS: &[&str] = &[
    "shell",
    "write_file",
    "edit_file",
    "multi_edit",
    "apply_patch",
    "copy_file",
    "explore",
    "create_agent",
];

/// Self-modification tools, layered on only by the `full` profile. These
/// re-wire the agent's own capabilities (installing packages, attaching
/// MCP servers) and are the most privileged class.
const SELF_MOD_TOOLS: &[&str] = &["install_packages", "add_mcp_server"];

/// A group's tool profile: the positive allow-list the agent is scoped
/// to. Profiles are cumulative — each tier adds to the one below it.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolProfile {
    /// Conversation + housekeeping only ([`ALWAYS_TOOLS`]). No file,
    /// shell, web, or scheduling access.
    Minimal,
    /// `Minimal` + read-only/informational tools ([`READONLY_TOOLS`]):
    /// read files, search the web, inspect git, manage schedules.
    Messaging,
    /// `Messaging` + filesystem mutation, shell, and `explore` /
    /// `create_agent` ([`CODING_TOOLS`]). A full development agent
    /// minus self-modification.
    Coding,
    /// Everything: `Coding` + self-modification ([`SELF_MOD_TOOLS`]).
    /// The historical default — applied to groups with no explicit
    /// profile so existing deployments keep their full tool surface.
    #[default]
    Full,
}

impl ToolProfile {
    /// Stable lower-case identifier (matches the serde representation and
    /// the per-group config field).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Messaging => "messaging",
            Self::Coding => "coding",
            Self::Full => "full",
        }
    }

    /// Parse a profile identifier. Returns `None` for unknown values so
    /// the caller can decide on a fallback (the runner config falls back
    /// to [`ToolProfile::Full`] and logs a warning).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "minimal" => Some(Self::Minimal),
            "messaging" => Some(Self::Messaging),
            "coding" => Some(Self::Coding),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// Whether this profile's positive allow-list admits `tool`. `Full`
    /// admits every tool (subject to the floor and the other layers);
    /// the lower tiers admit their cumulative tool sets.
    #[must_use]
    pub fn allows(self, tool: &str) -> bool {
        if ALWAYS_TOOLS.contains(&tool) {
            return true;
        }
        match self {
            Self::Minimal => false,
            Self::Messaging => READONLY_TOOLS.contains(&tool),
            Self::Coding => READONLY_TOOLS.contains(&tool) || CODING_TOOLS.contains(&tool),
            // `Full` is an open allow-list: anything that isn't on the
            // host-owned floor is permitted (new MCP tools are usable
            // without touching the profile table).
            Self::Full => true,
        }
    }
}

/// Sender role as seen by the runner's dispatch gate. Mirrors
/// [`copperclaw_modules::permissions::Role`] but is duplicated here to
/// keep the runner free of a `copperclaw-modules` dependency; the two
/// share the same lower-case string wire form.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SenderRole {
    Admin,
    Member,
    Guest,
}

impl SenderRole {
    /// Stable lower-case identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
            Self::Guest => "guest",
        }
    }

    /// Parse a role identifier (shares the wire form with
    /// `copperclaw_modules::permissions::Role`).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            "guest" => Some(Self::Guest),
            _ => None,
        }
    }

    /// Whether a sender with this role is barred from mutating tools
    /// (shell, file writes, self-mod) regardless of the active profile.
    /// Guests are read-only; members and admins are not held back here
    /// (the profile still bounds them).
    #[must_use]
    pub fn denies_mutating(self) -> bool {
        matches!(self, Self::Guest)
    }
}

/// Tool names a guest sender is never allowed to invoke — the read-only
/// floor. A guest may use [`ALWAYS_TOOLS`] + [`READONLY_TOOLS`] but never
/// the mutating / self-mod classes.
fn is_mutating(tool: &str) -> bool {
    CODING_TOOLS.contains(&tool) || SELF_MOD_TOOLS.contains(&tool)
}

/// Outcome of a policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// The tool call may proceed to dispatch.
    Allow,
    /// The tool call is refused; the string is a model-facing reason.
    Deny(String),
}

impl PolicyDecision {
    /// Convenience: `true` for [`PolicyDecision::Allow`].
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The deny reason, if this is a [`PolicyDecision::Deny`].
    #[must_use]
    pub fn deny_reason(&self) -> Option<&str> {
        match self {
            Self::Deny(reason) => Some(reason),
            Self::Allow => None,
        }
    }
}

/// Layered tool-authorization policy evaluated at every dispatch.
///
/// Construct with [`ToolPolicy::new`] (profile + optional sender role),
/// then narrow per-turn with [`ToolPolicy::with_active_skill`] when the
/// turn runs under a skill that declared `allowed-tools`. The default
/// ([`ToolPolicy::default`]) is permissive — `Full` profile, no role
/// gate, no skill scope — so existing call sites keep working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Group tool-profile (the positive allow-list ceiling).
    profile: ToolProfile,
    /// Resolved sender role, if the host supplied one. `None` means "do
    /// not apply the role floor" (profile + skill still apply).
    sender_role: Option<SenderRole>,
    /// Active skill's `allowed-tools`, if a skill that declared one is
    /// driving this turn. `None` means "no skill scope". When `Some`,
    /// only these names (plus [`ALWAYS_TOOLS`]) pass the skill layer.
    skill_allowed: Option<Vec<String>>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            profile: ToolProfile::Full,
            sender_role: None,
            skill_allowed: None,
        }
    }
}

impl ToolPolicy {
    /// Build a policy for a group profile and optional sender role.
    #[must_use]
    pub fn new(profile: ToolProfile, sender_role: Option<SenderRole>) -> Self {
        Self {
            profile,
            sender_role,
            skill_allowed: None,
        }
    }

    /// The group profile this policy enforces.
    #[must_use]
    pub fn profile(&self) -> ToolProfile {
        self.profile
    }

    /// The sender role floor this policy applies, if any.
    #[must_use]
    pub fn sender_role(&self) -> Option<SenderRole> {
        self.sender_role
    }

    /// Narrow the policy to a skill's `allowed-tools`. Returns a new
    /// policy; the original is left untouched (the runner clones the
    /// base policy per turn and applies the active skill).
    #[must_use]
    pub fn with_active_skill(mut self, allowed_tools: Option<Vec<String>>) -> Self {
        self.skill_allowed = allowed_tools;
        self
    }

    /// Evaluate `tool` against every layer. See the module docs for the
    /// ordering. Returns [`PolicyDecision::Allow`] only when the tool
    /// survives the floor *and* every positive layer.
    #[must_use]
    pub fn evaluate(&self, tool: &str) -> PolicyDecision {
        // Layer 1: host-owned floor. Always wins.
        if is_disallowed(tool) {
            return PolicyDecision::Deny(format!(
                "Tool `{tool}` is disallowed inside the copperclaw container (host-owned)."
            ));
        }

        // Layer 2: sender-role floor. A guest cannot invoke mutating
        // tools, period — even under a permissive profile.
        if let Some(role) = self.sender_role {
            if role.denies_mutating() && is_mutating(tool) {
                return PolicyDecision::Deny(format!(
                    "Tool `{tool}` is not available to `{}` senders (read-only).",
                    role.as_str()
                ));
            }
        }

        // Layer 3: active-skill `allowed-tools`. When a skill scoped the
        // turn, only its declared tools (plus the always-available
        // housekeeping set) pass.
        if let Some(allowed) = &self.skill_allowed {
            if !ALWAYS_TOOLS.contains(&tool) && !allowed.iter().any(|t| t == tool) {
                return PolicyDecision::Deny(format!(
                    "Tool `{tool}` is not in the active skill's allowed-tools list."
                ));
            }
        }

        // Layer 4: group profile ceiling.
        if !self.profile.allows(tool) {
            return PolicyDecision::Deny(format!(
                "Tool `{tool}` is not permitted by the `{}` tool profile.",
                self.profile.as_str()
            ));
        }

        PolicyDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_has_nine_entries() {
        // PLAN.md § 6 names exactly nine host-owned tools.
        assert_eq!(DISALLOWED_TOOLS.len(), 9);
    }

    #[test]
    fn every_floor_name_is_disallowed() {
        for name in DISALLOWED_TOOLS {
            assert!(is_disallowed(name), "expected disallowed: {name}");
        }
    }

    #[test]
    fn floor_is_case_sensitive() {
        assert!(is_disallowed("CronCreate"));
        assert!(!is_disallowed("croncreate"));
        assert!(!is_disallowed("CRONCREATE"));
        // Our MCP lower-case variants stay usable.
        for name in [
            "ask_user_question",
            "enter_plan_mode",
            "cron_create",
            "schedule_wakeup",
        ] {
            assert!(!is_disallowed(name), "unexpectedly disallowed: {name}");
        }
    }

    #[test]
    fn profile_str_roundtrip() {
        for p in [
            ToolProfile::Minimal,
            ToolProfile::Messaging,
            ToolProfile::Coding,
            ToolProfile::Full,
        ] {
            assert_eq!(ToolProfile::parse(p.as_str()), Some(p));
        }
        assert_eq!(ToolProfile::parse("nope"), None);
    }

    #[test]
    fn profile_default_is_full() {
        assert_eq!(ToolProfile::default(), ToolProfile::Full);
    }

    #[test]
    fn profile_serde_is_lowercase() {
        let json = serde_json::to_string(&ToolProfile::Coding).unwrap();
        assert_eq!(json, "\"coding\"");
        let back: ToolProfile = serde_json::from_str("\"messaging\"").unwrap();
        assert_eq!(back, ToolProfile::Messaging);
    }

    #[test]
    fn minimal_profile_allows_only_housekeeping() {
        let p = ToolProfile::Minimal;
        assert!(p.allows("send_message"));
        assert!(p.allows("todo_add"));
        assert!(!p.allows("read_file"));
        assert!(!p.allows("shell"));
        assert!(!p.allows("web_search"));
    }

    #[test]
    fn messaging_profile_allows_readonly_not_mutating() {
        let p = ToolProfile::Messaging;
        assert!(p.allows("send_message"));
        assert!(p.allows("read_file"));
        assert!(p.allows("web_search"));
        assert!(p.allows("grep"));
        assert!(!p.allows("shell"));
        assert!(!p.allows("write_file"));
        assert!(!p.allows("install_packages"));
    }

    #[test]
    fn coding_profile_allows_coding_tools() {
        let p = ToolProfile::Coding;
        assert!(p.allows("shell"));
        assert!(p.allows("write_file"));
        assert!(p.allows("apply_patch"));
        assert!(p.allows("explore"));
        assert!(p.allows("read_file")); // inherits messaging
        // self-mod is full-only
        assert!(!p.allows("install_packages"));
        assert!(!p.allows("add_mcp_server"));
    }

    #[test]
    fn full_profile_allows_everything_not_on_floor() {
        let p = ToolProfile::Full;
        assert!(p.allows("shell"));
        assert!(p.allows("install_packages"));
        assert!(p.allows("add_mcp_server"));
        // Even a tool we've never heard of passes the profile layer
        // under Full (the floor still catches host-owned names).
        assert!(p.allows("some_future_tool"));
    }

    #[test]
    fn role_str_roundtrip() {
        for r in [SenderRole::Admin, SenderRole::Member, SenderRole::Guest] {
            assert_eq!(SenderRole::parse(r.as_str()), Some(r));
        }
        assert_eq!(SenderRole::parse("nope"), None);
    }

    #[test]
    fn default_policy_is_permissive() {
        let p = ToolPolicy::default();
        assert_eq!(p.profile(), ToolProfile::Full);
        assert!(p.sender_role().is_none());
        assert!(p.evaluate("shell").is_allow());
        assert!(p.evaluate("send_message").is_allow());
    }

    #[test]
    fn floor_denied_under_every_profile_and_role() {
        for profile in [
            ToolProfile::Minimal,
            ToolProfile::Messaging,
            ToolProfile::Coding,
            ToolProfile::Full,
        ] {
            for role in [
                None,
                Some(SenderRole::Admin),
                Some(SenderRole::Member),
                Some(SenderRole::Guest),
            ] {
                let p = ToolPolicy::new(profile, role);
                let d = p.evaluate("CronCreate");
                assert!(
                    !d.is_allow(),
                    "floor should deny CronCreate ({profile:?}/{role:?})"
                );
                assert!(d.deny_reason().unwrap().contains("host-owned"));
            }
        }
    }

    #[test]
    fn guest_cannot_invoke_shell() {
        // A guest sender under an otherwise-permissive Full profile is
        // still barred from shell (and other mutating tools).
        let p = ToolPolicy::new(ToolProfile::Full, Some(SenderRole::Guest));
        let d = p.evaluate("shell");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("guest"));
        // …but can still read and message.
        assert!(p.evaluate("read_file").is_allow());
        assert!(p.evaluate("send_message").is_allow());
        // …and is barred from self-mod too.
        assert!(!p.evaluate("install_packages").is_allow());
        assert!(!p.evaluate("write_file").is_allow());
    }

    #[test]
    fn member_under_full_can_invoke_shell() {
        let p = ToolPolicy::new(ToolProfile::Full, Some(SenderRole::Member));
        assert!(p.evaluate("shell").is_allow());
        let admin = ToolPolicy::new(ToolProfile::Full, Some(SenderRole::Admin));
        assert!(admin.evaluate("shell").is_allow());
    }

    #[test]
    fn active_skill_allowed_tools_blocks_bash() {
        // A skill declaring `allowed-tools: [Read]` should block shell
        // even under a Coding profile and an admin sender.
        let p = ToolPolicy::new(ToolProfile::Coding, Some(SenderRole::Admin))
            .with_active_skill(Some(vec!["read_file".to_string()]));
        let d = p.evaluate("shell");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("active skill"));
        // The one allowed tool passes.
        assert!(p.evaluate("read_file").is_allow());
        // Housekeeping is always reachable even under a tight skill scope.
        assert!(p.evaluate("send_message").is_allow());
    }

    #[test]
    fn active_skill_none_does_not_restrict() {
        let p = ToolPolicy::new(ToolProfile::Coding, None).with_active_skill(None);
        assert!(p.evaluate("shell").is_allow());
    }

    #[test]
    fn full_profile_allows_coding_tools() {
        // Explicit task assertion: full profile permits coding tools.
        let p = ToolPolicy::new(ToolProfile::Full, None);
        for t in ["shell", "write_file", "edit_file", "apply_patch", "explore"] {
            assert!(p.evaluate(t).is_allow(), "full should allow {t}");
        }
    }

    #[test]
    fn profile_ceiling_denies_shell_under_messaging() {
        let p = ToolPolicy::new(ToolProfile::Messaging, Some(SenderRole::Admin));
        let d = p.evaluate("shell");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("messaging"));
    }

    #[test]
    fn skill_layer_intersects_with_profile() {
        // A skill may name a tool the profile forbids — the profile
        // ceiling still applies (intersection, not union).
        let p = ToolPolicy::new(ToolProfile::Messaging, None)
            .with_active_skill(Some(vec!["shell".to_string()]));
        // shell passes the skill layer but the messaging profile denies it.
        let d = p.evaluate("shell");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("messaging"));
    }
}
