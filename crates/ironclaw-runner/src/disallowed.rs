//! Disallowed built-in tool list (PLAN.md § 6 T5).
//!
//! These tools are owned by the host orchestrator and the runner must
//! refuse them with a synthetic error result. The list is enforced at the
//! runner's tool-dispatch layer (not by hiding the names from the model).
//!
//! Note: `ask_user_question` is an MCP tool we own and is *allowed*. The
//! disallow list targets the historical built-in tool name
//! `AskUserQuestion` (pascal-case) — matching is case-sensitive on purpose
//! so that the lower-case MCP variant remains usable.

/// Built-in tool names the runner must refuse.
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

/// Returns `true` if `name` exactly matches a disallowed tool. Matching is
/// case-sensitive: lower-case variants (e.g. our MCP `ask_user_question`)
/// are *not* on the list.
#[must_use]
pub fn is_disallowed(name: &str) -> bool {
    DISALLOWED_TOOLS.iter().any(|t| *t == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_has_nine_entries() {
        // PLAN.md § 6 names exactly nine tools.
        assert_eq!(DISALLOWED_TOOLS.len(), 9);
    }

    #[test]
    fn every_listed_name_is_disallowed() {
        for name in DISALLOWED_TOOLS {
            assert!(is_disallowed(name), "expected disallowed: {name}");
        }
    }

    #[test]
    fn lowercase_variants_are_allowed() {
        // Our MCP `ask_user_question` (lower-case, owned by ironclaw-mcp) must
        // *not* be on the list. Disallowing it would break the legitimate tool.
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
    fn unknown_tool_is_allowed() {
        assert!(!is_disallowed("bash"));
        assert!(!is_disallowed("send_message"));
        assert!(!is_disallowed(""));
    }

    #[test]
    fn case_sensitive() {
        assert!(is_disallowed("CronCreate"));
        assert!(!is_disallowed("croncreate"));
        assert!(!is_disallowed("CRONCREATE"));
    }
}
