//! Host-owned disallowed tool list (PLAN.md § 6 T5).
//!
//! This module is the historical home of the floor list. It is now a thin
//! compatibility re-export: the canonical definitions live in
//! [`crate::policy`], where they form layer 1 (the host-owned floor) of
//! the layered [`crate::policy::ToolPolicy`] gate. Existing callers that
//! import [`DISALLOWED_TOOLS`] / [`is_disallowed`] from here keep working.
//!
//! Note: `ask_user_question` is an MCP tool we own and is *allowed*. The
//! disallow list targets the historical built-in tool name
//! `AskUserQuestion` (pascal-case) — matching is case-sensitive on purpose
//! so that the lower-case MCP variant remains usable.

pub use crate::policy::{DISALLOWED_TOOLS, is_disallowed};

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
        // Our MCP `ask_user_question` (lower-case, owned by copperclaw-mcp) must
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
