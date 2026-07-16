//! Tool authorization policy (PLAN.md § 6 T5, security-hardening Phase 1.1).
//!
//! A layered gate evaluated at every tool dispatch (see
//! [`crate::run::tool_dispatch::invoke_tool`]). A tool call is permitted
//! only when it survives, in order:
//!
//! 1. **Sender role.** A [`SenderRole::Guest`] sender is held to a
//!    read-only floor: shell / file-mutation / self-modification tools
//!    are denied even if the active profile would otherwise allow them.
//! 2. **Active skill `allowed-tools`.** When the turn is running under a
//!    skill that declared an `allowed-tools` frontmatter list, only the
//!    named tools (plus the always-available housekeeping set) pass —
//!    so a skill declaring `allowed-tools: [Read]` blocks `shell`.
//! 3. **Group tool-profile.** The positive allow-list for the group:
//!    [`ToolProfile::Minimal`] / [`Messaging`] / [`Coding`] / [`Full`].
//! 4. **Coarse provenance / autonomy gate.** Credentialed external
//!    actions (see [`is_credentialed_external`]) are blocked on a
//!    tainted turn until a fresh approval, and blocked outright on an
//!    autonomous turn (see [`TurnTrust`]).
//!
//! Layers 1-3 are a positive allow-list (default-deny intersection);
//! layer 4 is a conditional gate over a small set of external actions.
//!
//! ## Where the `DISALLOWED_TOOLS` floor went (M18 R0)
//!
//! Until M18 this module also carried a "host-owned floor": a hard
//! deny-list of nine historical pascal-case built-in tool names
//! (`CronCreate`, `AskUserQuestion`, `EnterPlanMode`, ... — PLAN.md § 6
//! T5) inherited from the upstream design, evaluated before every other
//! layer. Those names never matched the runner's actual `snake_case` tool
//! inventory ([`copperclaw_mcp::build_tool_set`]), so the floor never
//! denied a real call — every effective deny came from the role / skill
//! / profile / provenance layers above. The list was deleted rather than
//! repopulated: every first-party tool is intentionally reachable under
//! the `Full` profile, so there is no in-tree name an unconditional
//! deny-list should pin. **The profiles are the enforcement mechanism.**
//! A name that matches no registered tool still fails at dispatch with
//! an "Unknown tool" error (`crate::run::tool_dispatch`). To keep the
//! profile lists from rotting the same way the floor did, the
//! `tool_name_drift` integration test pins every name referenced by
//! [`PROFILE_TOOL_LISTS`] to the real inventory.
//!
//! [`Messaging`]: ToolProfile::Messaging
//! [`Coding`]: ToolProfile::Coding
//! [`Full`]: ToolProfile::Full
//! [`copperclaw_mcp::build_tool_set`]: copperclaw_mcp::build_tool_set

use serde::{Deserialize, Serialize};

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
/// guest sender: they observe but never mutate the filesystem, shell, or
/// scheduler. `list_tasks` is the only scheduling tool here — it reads the
/// task list without changing it. The mutating scheduling verbs live in
/// [`SCHEDULING_MUTATION_TOOLS`] so the guest role floor denies them.
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
];

/// Scheduler-mutation verbs layered on top of [`READONLY_TOOLS`] by the
/// `messaging` profile (and inherited by richer profiles). These create,
/// cancel, pause, resume, or edit scheduled tasks — they mutate scheduler
/// state, so they are classified as mutating (see [`is_mutating`]) and a
/// [`SenderRole::Guest`] sender is denied them even though the messaging
/// profile would otherwise admit them. (Wave-2 nit: previously these sat
/// in `READONLY_TOOLS`, which let a guest mutate the scheduler.)
const SCHEDULING_MUTATION_TOOLS: &[&str] = &[
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
    "delegate",
    // M17 session-preview proxy: exposing / closing an HTTP app the agent
    // built is part of the build-test loop, so it rides the coding profile
    // (and is denied to a guest via the mutating floor — see `is_mutating`).
    "expose_preview",
    "close_preview",
];

/// Self-modification tools, layered on only by the `full` profile. These
/// re-wire the agent's own capabilities (installing packages, attaching
/// MCP servers, saving reusable skills) and are the most privileged class.
///
/// `save_skill` (M19 A4) persists an agent-authored skill into the group's
/// per-group skills override for the next spawn to discover. It is self-mod
/// (it durably changes the agent's own capability surface) but — unlike
/// `install_packages` / `add_mcp_server` — it does NOT egress, so it is
/// deliberately absent from [`CREDENTIALED_EXTERNAL_TOOLS`]. Its
/// secure-by-default gate is the host-side operator approval raised before
/// the skill is ever written.
const SELF_MOD_TOOLS: &[&str] = &["install_packages", "add_mcp_server", "save_skill"];

/// Memory-*write* tools (M19 A5). `memory_search` / `memory_get` are read-only
/// and live outside every profile list (Full-only, like their write sibling),
/// but `memory_save` MUTATES the group memory store, so it is classified as
/// mutating here: a [`SenderRole::Guest`] sender is denied it (see
/// [`is_mutating`]) — a guest must not be able to write a `trusted` fact into
/// the store. Provenance honesty against content-taint is enforced separately
/// in the runner's `memory_save` impl (a tainted turn is forced to
/// `untrusted`).
const MEMORY_WRITE_TOOLS: &[&str] = &["memory_save"];

/// Tools that take a **credentialed external action** — they reach outside the
/// container over the network (the egress path the credential broker meters)
/// to fetch data, run a search, install packages, or attach a remote MCP
/// server. These are exactly the actions the coarse provenance gate guards:
///
///   - On a turn whose context contains ANY untrusted-provenance content (a
///     `web_fetch` body, an untrusted memory hit), these are blocked until a
///     fresh approval clears the taint — the "confused-deputy" defence against
///     prompt injection routing the agent's credentials at an attacker target.
///   - On an autonomous / heartbeat turn (no human in the loop), these are
///     blocked outright: an autonomous turn may *search memory and propose* but
///     may not *take* a credentialed external action without a human turn to
///     approve it (read-then-propose).
///
/// `web_search` and `web_fetch` are the egress-bearing read tools; the self-mod
/// tools fetch remote packages / attach remote servers. This list is the
/// runner's policy view — it does NOT need to enumerate every future MCP tool,
/// only the in-tree ones that egress on the broker's dime.
const CREDENTIALED_EXTERNAL_TOOLS: &[&str] = &[
    "web_fetch",
    "web_search",
    "install_packages",
    "add_mcp_server",
    // M17 session-preview proxy: `expose_preview` stands up a LAN-reachable
    // listener on the operator's network — an external action. It obeys the
    // same provenance / autonomy gate as `web_fetch`: blocked outright on an
    // autonomous turn, and blocked on a tainted turn until a fresh approval
    // clears it, so a prompt-injected turn can't publish an attacker-chosen
    // app to the LAN. `close_preview` is the paired teardown; gating it too
    // keeps the pair symmetric and harmless (tearing down is safe, but the
    // block only bites on already-tainted/autonomous turns).
    "expose_preview",
    "close_preview",
];

/// Every tool-name list this policy references, labelled for diagnostics.
///
/// This is the drift-guard surface: the `tool_name_drift` integration test
/// asserts every name here exists in the real in-container inventory
/// ([`copperclaw_mcp::build_tool_set`]) or is one of the host-brokered
/// preview tools relayed via the `__preview` server
/// (`crate::run::preview`). The old `DISALLOWED_TOOLS` floor rotted
/// precisely because nothing pinned its names to the inventory (see the
/// module docs); this export exists so the same drift in the profile
/// lists fails CI instead.
///
/// [`copperclaw_mcp::build_tool_set`]: copperclaw_mcp::build_tool_set
pub const PROFILE_TOOL_LISTS: &[(&str, &[&str])] = &[
    ("ALWAYS_TOOLS", ALWAYS_TOOLS),
    ("READONLY_TOOLS", READONLY_TOOLS),
    ("SCHEDULING_MUTATION_TOOLS", SCHEDULING_MUTATION_TOOLS),
    ("CODING_TOOLS", CODING_TOOLS),
    ("SELF_MOD_TOOLS", SELF_MOD_TOOLS),
    ("MEMORY_WRITE_TOOLS", MEMORY_WRITE_TOOLS),
    ("CREDENTIALED_EXTERNAL_TOOLS", CREDENTIALED_EXTERNAL_TOOLS),
];

/// Namespace prefix the runner gives every **external** MCP tool it advertises
/// (`mcp__<server>__<tool>`). Mirrors the `mcp__server__tool` convention used
/// elsewhere and keeps external tools from colliding with first-party names.
pub const EXTERNAL_MCP_PREFIX: &str = "mcp__";

/// True when `tool` takes a credentialed external action (see
/// [`CREDENTIALED_EXTERNAL_TOOLS`]).
///
/// External MCP tools (the `mcp__`-prefixed names the runner advertises from a
/// group's configured external servers) are treated as credentialed external
/// actions too: the host executes them over the network on the broker's dime,
/// so they must obey the same provenance / autonomy gate as `web_fetch` —
/// blocked outright on an autonomous turn, and blocked on a tainted turn until
/// a fresh approval clears it. This closes the confused-deputy hole where a
/// prompt-injected turn could route the agent at an attacker-chosen external
/// MCP tool to exfiltrate.
#[must_use]
pub fn is_credentialed_external(tool: &str) -> bool {
    CREDENTIALED_EXTERNAL_TOOLS.contains(&tool) || tool.starts_with(EXTERNAL_MCP_PREFIX)
}

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
    /// admits every tool (subject to the other policy layers); the lower
    /// tiers admit their cumulative tool sets.
    #[must_use]
    pub fn allows(self, tool: &str) -> bool {
        if ALWAYS_TOOLS.contains(&tool) {
            return true;
        }
        match self {
            Self::Minimal => false,
            Self::Messaging => {
                READONLY_TOOLS.contains(&tool) || SCHEDULING_MUTATION_TOOLS.contains(&tool)
            }
            Self::Coding => {
                READONLY_TOOLS.contains(&tool)
                    || SCHEDULING_MUTATION_TOOLS.contains(&tool)
                    || CODING_TOOLS.contains(&tool)
            }
            // `Full` is an open allow-list: everything is permitted at
            // the profile layer (new MCP tools are usable without
            // touching the profile table); a name that matches no
            // registered tool still fails at dispatch as unknown.
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
/// the mutating classes: scheduler-mutation verbs, filesystem/shell
/// (`CODING_TOOLS`), or self-modification (`SELF_MOD_TOOLS`).
fn is_mutating(tool: &str) -> bool {
    SCHEDULING_MUTATION_TOOLS.contains(&tool)
        || CODING_TOOLS.contains(&tool)
        || SELF_MOD_TOOLS.contains(&tool)
        || MEMORY_WRITE_TOOLS.contains(&tool)
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
    /// Coarse provenance gate (M16 Phase 3). Set per-call by the dispatch
    /// gate from the live turn state. See [`TurnTrust`].
    trust: TurnTrust,
}

/// Per-turn trust state feeding the coarse provenance gate (Phase 3).
///
/// Built fresh per dispatch from the live turn: whether the context has been
/// tainted by untrusted-provenance content this turn, whether a fresh approval
/// has cleared that taint, and whether this is an autonomous (heartbeat /
/// scheduled) turn with no human in the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnTrust {
    /// True once any untrusted-provenance content (a `web_fetch` body, an
    /// untrusted memory hit) has entered this turn's context.
    pub tainted: bool,
    /// True when the operator has granted a fresh approval for credentialed
    /// external actions on this (tainted) turn. Clears the taint block.
    pub approved: bool,
    /// True when this is an autonomous turn (heartbeat / scheduled wake) with
    /// no triggering human message — read-then-propose only.
    pub autonomous: bool,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            profile: ToolProfile::Full,
            sender_role: None,
            skill_allowed: None,
            trust: TurnTrust::default(),
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
            trust: TurnTrust::default(),
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

    /// Apply the per-turn [`TurnTrust`] for the coarse provenance gate
    /// (Phase 3). Returns a new policy; the original is untouched (the
    /// dispatch gate clones the base policy per call and stamps the live
    /// trust state, exactly like [`Self::with_active_skill`]).
    #[must_use]
    pub fn with_trust(mut self, trust: TurnTrust) -> Self {
        self.trust = trust;
        self
    }

    /// Evaluate `tool` against every layer. See the module docs for the
    /// ordering. Returns [`PolicyDecision::Allow`] only when the tool
    /// survives every layer.
    #[must_use]
    pub fn evaluate(&self, tool: &str) -> PolicyDecision {
        // Layer 1: sender-role floor. A guest cannot invoke mutating
        // tools, period — even under a permissive profile.
        if let Some(role) = self.sender_role {
            if role.denies_mutating() && is_mutating(tool) {
                copperclaw_metrics::inc_policy_denied("role", tool);
                return PolicyDecision::Deny(format!(
                    "Tool `{tool}` is not available to `{}` senders (read-only).",
                    role.as_str()
                ));
            }
        }

        // Layer 2: active-skill `allowed-tools`. When a skill scoped the
        // turn, only its declared tools (plus the always-available
        // housekeeping set) pass.
        if let Some(allowed) = &self.skill_allowed {
            if !ALWAYS_TOOLS.contains(&tool) && !allowed.iter().any(|t| t == tool) {
                copperclaw_metrics::inc_policy_denied("skill", tool);
                return PolicyDecision::Deny(format!(
                    "Tool `{tool}` is not in the active skill's allowed-tools list."
                ));
            }
        }

        // Layer 3: group profile ceiling.
        if !self.profile.allows(tool) {
            copperclaw_metrics::inc_policy_denied("profile", tool);
            return PolicyDecision::Deny(format!(
                "Tool `{tool}` is not permitted by the `{}` tool profile.",
                self.profile.as_str()
            ));
        }

        // Layer 4: coarse provenance / autonomy gate (Phase 3). Only
        // credentialed external actions are gated here — memory search,
        // messaging, and local tools always pass so an autonomous turn can
        // still read-then-propose.
        if is_credentialed_external(tool) {
            // Autonomous (heartbeat / scheduled) turns may NOT take a
            // credentialed external action at all — no human is present to
            // authorise it. They may still search memory and propose.
            if self.trust.autonomous {
                copperclaw_metrics::inc_policy_denied("provenance", tool);
                return PolicyDecision::Deny(format!(
                    "Tool `{tool}` takes a credentialed external action, which is not permitted on an autonomous (heartbeat/scheduled) turn. Search memory and propose the action for a human turn to approve instead."
                ));
            }
            // A tainted turn (context touched untrusted-provenance content,
            // e.g. a web_fetch body or an untrusted memory hit) blocks
            // credentialed external actions until a FRESH approval clears it.
            if self.trust.tainted && !self.trust.approved {
                copperclaw_metrics::inc_policy_denied("provenance", tool);
                return PolicyDecision::Deny(format!(
                    "Tool `{tool}` takes a credentialed external action, but this turn's context contains untrusted-provenance content (e.g. a fetched page or an untrusted memory entry). Fresh approval is required before a credentialed external action can run on a tainted turn."
                ));
            }
        }

        PolicyDecision::Allow
    }

    /// The per-turn trust state this policy enforces.
    #[must_use]
    pub fn trust(&self) -> TurnTrust {
        self.trust
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_tool_lists_cover_every_policy_list() {
        // The drift-guard export must carry all lists (the
        // `tool_name_drift` integration test iterates it; a list missing
        // here escapes the guard).
        let labels: Vec<&str> = PROFILE_TOOL_LISTS.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            labels,
            vec![
                "ALWAYS_TOOLS",
                "READONLY_TOOLS",
                "SCHEDULING_MUTATION_TOOLS",
                "CODING_TOOLS",
                "SELF_MOD_TOOLS",
                "MEMORY_WRITE_TOOLS",
                "CREDENTIALED_EXTERNAL_TOOLS",
            ]
        );
        for (label, names) in PROFILE_TOOL_LISTS {
            assert!(!names.is_empty(), "{label} is empty");
        }
    }

    #[test]
    fn profile_tool_names_are_snake_case() {
        // The old DISALLOWED_TOOLS floor rotted because it carried
        // pascal-case names that never matched the snake_case inventory.
        // Keep pascal-case (or any other casing) from creeping into the
        // live lists.
        for (label, names) in PROFILE_TOOL_LISTS {
            for name in *names {
                assert!(
                    name.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{label} entry `{name}` is not snake_case"
                );
            }
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
    fn full_profile_allows_everything() {
        let p = ToolProfile::Full;
        assert!(p.allows("shell"));
        assert!(p.allows("install_packages"));
        assert!(p.allows("add_mcp_server"));
        // Even a tool we've never heard of passes the profile layer
        // under Full — an unregistered name fails at dispatch as
        // "Unknown tool" instead (there is no deny-list floor).
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
    fn unknown_tool_denied_under_every_non_full_profile() {
        // With the decorative DISALLOWED_TOOLS floor gone, the profile
        // allow-lists are the mechanism: a name outside the lists is
        // denied by every profile except the open `Full` allow-list
        // (where an unregistered name fails at dispatch as unknown).
        for profile in [
            ToolProfile::Minimal,
            ToolProfile::Messaging,
            ToolProfile::Coding,
        ] {
            for role in [
                None,
                Some(SenderRole::Admin),
                Some(SenderRole::Member),
                Some(SenderRole::Guest),
            ] {
                let p = ToolPolicy::new(profile, role);
                let d = p.evaluate("some_unknown_tool");
                assert!(
                    !d.is_allow(),
                    "{profile:?}/{role:?} should deny an unknown tool"
                );
                assert!(d.deny_reason().unwrap().contains(profile.as_str()));
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
    fn guest_cannot_mutate_scheduler() {
        // Wave-2 nit: scheduling-mutation verbs must be denied to a guest
        // even under a messaging profile (which admits them for higher
        // roles). `list_tasks` (read-only) stays reachable.
        let p = ToolPolicy::new(ToolProfile::Messaging, Some(SenderRole::Guest));
        for verb in [
            "schedule_task",
            "cancel_task",
            "pause_task",
            "resume_task",
            "update_task",
        ] {
            let d = p.evaluate(verb);
            assert!(!d.is_allow(), "guest should be denied {verb}");
            assert!(d.deny_reason().unwrap().contains("guest"), "{verb}");
        }
        assert!(
            p.evaluate("list_tasks").is_allow(),
            "list_tasks is read-only and stays reachable for a guest"
        );
    }

    #[test]
    fn member_can_mutate_scheduler_under_messaging() {
        // The scheduling verbs are still available to a non-guest sender on
        // a messaging-profile group — the Wave-2 fix only closes the guest
        // hole, it does not remove the capability for members/admins.
        let p = ToolPolicy::new(ToolProfile::Messaging, Some(SenderRole::Member));
        for verb in [
            "schedule_task",
            "cancel_task",
            "pause_task",
            "resume_task",
            "update_task",
            "list_tasks",
        ] {
            assert!(p.evaluate(verb).is_allow(), "member should allow {verb}");
        }
        // …but shell is still blocked by the messaging profile ceiling.
        assert!(!p.evaluate("shell").is_allow());
    }

    #[test]
    fn scheduling_verbs_still_allowed_by_messaging_profile() {
        // Profile-layer check (no role floor): the messaging profile admits
        // the scheduling verbs (moving them out of READONLY_TOOLS must not
        // drop them from the profile's allow-list).
        let p = ToolProfile::Messaging;
        for verb in [
            "schedule_task",
            "cancel_task",
            "pause_task",
            "resume_task",
            "update_task",
            "list_tasks",
        ] {
            assert!(p.allows(verb), "messaging profile should allow {verb}");
        }
    }

    // ── coarse provenance / autonomy gate (Phase 3) ──────────────────────

    #[test]
    fn credentialed_external_set_is_what_we_expect() {
        for t in [
            "web_fetch",
            "web_search",
            "install_packages",
            "add_mcp_server",
        ] {
            assert!(
                is_credentialed_external(t),
                "{t} should be credentialed-external"
            );
        }
        for t in [
            "read_file",
            "send_message",
            "memory_search",
            "memory_get",
            "shell",
        ] {
            assert!(
                !is_credentialed_external(t),
                "{t} must not be credentialed-external"
            );
        }
    }

    #[test]
    fn memory_save_is_mutating_and_denied_to_guests() {
        // A5: memory_save writes the group store, so a guest (read-only) sender
        // is denied it even under the open Full profile — a guest must not
        // launder a `trusted` fact into memory. memory_search/get stay allowed.
        assert!(is_mutating("memory_save"));
        assert!(!is_mutating("memory_search"));
        let guest = ToolPolicy::new(ToolProfile::Full, Some(SenderRole::Guest));
        assert!(!guest.evaluate("memory_save").is_allow());
        assert!(guest.evaluate("memory_search").is_allow());
        // A full member can write.
        let member = ToolPolicy::new(ToolProfile::Full, None);
        assert!(member.evaluate("memory_save").is_allow());
    }

    #[test]
    fn untrusted_context_blocks_credentialed_external_without_approval() {
        // Headline Phase 3 case: a turn whose context touched untrusted
        // content blocks a credentialed external action absent fresh approval.
        let tainted = ToolPolicy::new(ToolProfile::Full, None).with_trust(TurnTrust {
            tainted: true,
            approved: false,
            autonomous: false,
        });
        let d = tainted.evaluate("web_fetch");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("untrusted-provenance"));
        // Non-credentialed tools still pass on a tainted turn — the agent can
        // read memory and propose.
        assert!(tainted.evaluate("memory_search").is_allow());
        assert!(tainted.evaluate("read_file").is_allow());
        assert!(tainted.evaluate("send_message").is_allow());
    }

    #[test]
    fn fresh_approval_clears_taint_for_credentialed_external() {
        let approved = ToolPolicy::new(ToolProfile::Full, None).with_trust(TurnTrust {
            tainted: true,
            approved: true,
            autonomous: false,
        });
        assert!(
            approved.evaluate("web_fetch").is_allow(),
            "a fresh approval must clear the taint block"
        );
        assert!(approved.evaluate("install_packages").is_allow());
    }

    #[test]
    fn untainted_turn_allows_credentialed_external() {
        let clean = ToolPolicy::new(ToolProfile::Full, None);
        assert!(clean.evaluate("web_fetch").is_allow());
        assert!(clean.evaluate("web_search").is_allow());
        assert!(clean.evaluate("add_mcp_server").is_allow());
    }

    #[test]
    fn external_mcp_tools_are_credentialed_external() {
        // The `mcp__<server>__<tool>` namespace marks a host-proxied external
        // MCP call, which egresses on the broker's dime — gated like web_fetch.
        assert!(is_credentialed_external("mcp__weather__forecast"));
        assert!(is_credentialed_external("mcp__gh__create_issue"));
        // First-party / local tools are NOT external.
        assert!(!is_credentialed_external("read_file"));
        assert!(!is_credentialed_external("send_message"));
        // A name that merely contains "mcp" but lacks the `mcp__` prefix is not
        // treated as an external MCP call (only the namespaced form is).
        assert!(!is_credentialed_external("inspect_mcp_filter"));
    }

    #[test]
    fn external_mcp_call_blocked_on_autonomous_turn() {
        // An autonomous turn may not take a credentialed external action — and
        // an external MCP tool is one. Under Full it would otherwise pass.
        let auto = ToolPolicy::new(ToolProfile::Full, None).with_trust(TurnTrust {
            tainted: false,
            approved: false,
            autonomous: true,
        });
        let d = auto.evaluate("mcp__weather__forecast");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("autonomous"));
    }

    #[test]
    fn external_mcp_call_blocked_on_tainted_turn_until_approved() {
        let tainted = ToolPolicy::new(ToolProfile::Full, None).with_trust(TurnTrust {
            tainted: true,
            approved: false,
            autonomous: false,
        });
        assert!(
            tainted
                .evaluate("mcp__weather__forecast")
                .deny_reason()
                .unwrap()
                .contains("untrusted-provenance")
        );
        // Fresh approval clears it.
        let approved = ToolPolicy::new(ToolProfile::Full, None).with_trust(TurnTrust {
            tainted: true,
            approved: true,
            autonomous: false,
        });
        assert!(approved.evaluate("mcp__weather__forecast").is_allow());
    }

    #[test]
    fn external_mcp_call_passes_on_clean_full_turn() {
        let clean = ToolPolicy::new(ToolProfile::Full, None);
        assert!(clean.evaluate("mcp__weather__forecast").is_allow());
    }

    #[test]
    fn autonomous_turn_blocks_credentialed_external_even_when_clean() {
        // An autonomous (heartbeat) turn may search memory and propose, but
        // never *take* a credentialed external action — even with no taint.
        let auto = ToolPolicy::new(ToolProfile::Full, None).with_trust(TurnTrust {
            tainted: false,
            approved: false,
            autonomous: true,
        });
        let d = auto.evaluate("web_fetch");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("autonomous"));
        assert!(!auto.evaluate("web_search").is_allow());
        assert!(!auto.evaluate("install_packages").is_allow());
        // Memory + messaging stay reachable (read-then-propose).
        assert!(auto.evaluate("memory_search").is_allow());
        assert!(auto.evaluate("memory_get").is_allow());
        assert!(auto.evaluate("send_message").is_allow());
    }

    #[test]
    fn autonomous_block_is_not_cleared_by_approval_field() {
        // The autonomous block is unconditional — `approved` only clears the
        // taint block, not the autonomous one (no human turn to approve on).
        let auto = ToolPolicy::new(ToolProfile::Full, None).with_trust(TurnTrust {
            tainted: true,
            approved: true,
            autonomous: true,
        });
        assert!(!auto.evaluate("web_fetch").is_allow());
        assert!(
            auto.evaluate("web_fetch")
                .deny_reason()
                .unwrap()
                .contains("autonomous")
        );
    }

    #[test]
    fn default_policy_has_clean_trust() {
        assert_eq!(ToolPolicy::default().trust(), TurnTrust::default());
        assert!(!TurnTrust::default().tainted);
        assert!(!TurnTrust::default().autonomous);
    }

    // ── M17 preview tools policy ─────────────────────────────────────────

    #[test]
    fn preview_tools_are_coding_profile_and_credentialed_external() {
        for t in ["expose_preview", "close_preview"] {
            // Coding + full profiles admit them; minimal/messaging do not.
            assert!(ToolProfile::Coding.allows(t), "coding should allow {t}");
            assert!(ToolProfile::Full.allows(t), "full should allow {t}");
            assert!(!ToolProfile::Minimal.allows(t), "minimal must deny {t}");
            assert!(!ToolProfile::Messaging.allows(t), "messaging must deny {t}");
            // They take a credentialed external action (LAN listener).
            assert!(
                is_credentialed_external(t),
                "{t} must be credentialed-external"
            );
        }
    }

    #[test]
    fn preview_tools_denied_to_guest() {
        // A guest under an otherwise-permissive Full profile is barred (they're
        // mutating tools via CODING_TOOLS).
        let p = ToolPolicy::new(ToolProfile::Full, Some(SenderRole::Guest));
        for t in ["expose_preview", "close_preview"] {
            let d = p.evaluate(t);
            assert!(!d.is_allow(), "guest should be denied {t}");
            assert!(d.deny_reason().unwrap().contains("guest"), "{t}");
        }
    }

    #[test]
    fn preview_expose_denied_on_tainted_turn_without_approval() {
        let tainted =
            ToolPolicy::new(ToolProfile::Coding, Some(SenderRole::Admin)).with_trust(TurnTrust {
                tainted: true,
                approved: false,
                autonomous: false,
            });
        let d = tainted.evaluate("expose_preview");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("untrusted-provenance"));
        // A fresh approval clears it.
        let approved =
            ToolPolicy::new(ToolProfile::Coding, Some(SenderRole::Admin)).with_trust(TurnTrust {
                tainted: true,
                approved: true,
                autonomous: false,
            });
        assert!(approved.evaluate("expose_preview").is_allow());
    }

    #[test]
    fn preview_expose_blocked_on_autonomous_turn() {
        let auto = ToolPolicy::new(ToolProfile::Coding, None).with_trust(TurnTrust {
            tainted: false,
            approved: false,
            autonomous: true,
        });
        let d = auto.evaluate("expose_preview");
        assert!(!d.is_allow());
        assert!(d.deny_reason().unwrap().contains("autonomous"));
    }

    #[test]
    fn preview_expose_allowed_on_clean_coding_turn() {
        let clean = ToolPolicy::new(ToolProfile::Coding, Some(SenderRole::Member));
        assert!(clean.evaluate("expose_preview").is_allow());
        assert!(clean.evaluate("close_preview").is_allow());
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
