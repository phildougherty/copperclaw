//! `save_skill`: persist an agent-authored reusable skill (M19 A4).
//!
//! Closes the `write_file` → discovery loop. Today an agent can `write_file` a
//! `SKILL.md`, but nothing re-discovers or exposes it — skills are host-
//! discovered and symlink-materialized only at spawn. This tool lets an agent
//! that has worked out a good repeatable procedure durably save it as a skill
//! the *next* session will discover.
//!
//! Secure-by-default and approval-gated. The tool itself does two things:
//!
//! 1. **Validates** the proposed `SKILL.md` against the exact discovery-time
//!    rules ([`copperclaw_skills::frontmatter`] + kebab-case name + `name ==
//!    dir`) so an invalid skill is refused *here*, synchronously, with the
//!    precise validation error — never queued for an operator only to fail.
//! 2. On valid input, **emits** an [`OutboundToolEffect::SaveSkill`]. The
//!    runner records it and the host raises an approval; only on operator
//!    approval does the `SKILL.md` land in the group's per-group skills
//!    override directory (`<groups_dir>/<agent_group_id>/skills/<name>/`),
//!    where the next spawn discovers it.
//!
//! This is a **capability, not a registry**: a saved skill is per-group only.
//! There is no cross-group sharing and no `ClawHub` (a standing non-goal).

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

use crate::context::{OutboundToolEffect, SaveSkillSpec, ToolContext};
use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args};

#[derive(Debug, Deserialize)]
struct Input {
    name: String,
    content: String,
    reason: String,
}

pub fn schema() -> Tool {
    make_tool(
        "save_skill",
        "Save a reusable skill so a FUTURE session discovers it. Provide `name` \
         (kebab-case, must equal the SKILL.md frontmatter `name`), `content` \
         (the full SKILL.md text including its `---` YAML frontmatter with \
         `name` and `description`), and `reason`. The skill is validated now \
         and, on operator approval, written into this group's skills so the \
         next session can use it. Per-group only — not shared across groups. \
         Saving is VERSION-AWARE: a first save is version 1 (or the `version` \
         you set in the frontmatter), and re-saving an existing skill of the \
         same name bumps its version automatically — you do not need to manage \
         `version` yourself. Use `list_skills` to see saved skills and their \
         versions. Use this to durably teach yourself a repeatable procedure.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["name", "content", "reason"],
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Kebab-case skill name (matches the SKILL.md frontmatter `name`; becomes the directory slug)."
                },
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The full SKILL.md text, including the `---` YAML frontmatter (`name`, `description`, optional `allowed-tools`, optional `version` — managed automatically on re-save)."
                },
                "reason": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Why this skill is worth saving (shown on the approval)."
                }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ToolError::Validation("`name` must be non-empty".into()));
    }
    if input.content.trim().is_empty() {
        return Err(ToolError::Validation("`content` must be non-empty".into()));
    }
    if input.reason.trim().is_empty() {
        return Err(ToolError::Validation("`reason` must be non-empty".into()));
    }

    // Validate synchronously against the discovery-time rules so an invalid
    // skill is refused HERE with the precise error, before any approval is
    // raised. Reuses the exact frontmatter + name checks the registry applies
    // at load time (parse frontmatter, kebab-case name, frontmatter `name` ==
    // requested name / on-disk dir slug). Pure — no filesystem write; the
    // approved host side re-runs the same validation before it writes.
    copperclaw_skills::validate_skill_content(&name, &input.content)
        .map_err(|e| ToolError::Validation(e.to_string()))?;

    let spec = SaveSkillSpec {
        name: name.clone(),
        content: input.content,
        reason: input.reason,
    };
    ctx.emit_outbound(OutboundToolEffect::SaveSkill(spec))
        .await?;

    Ok(CallToolResult::success(vec![Content::text(format!(
        "Skill `{name}` validated and submitted for approval. Once an operator \
         approves, it lands in this group's skills and becomes available in your \
         NEXT session (not the current one). If a skill of this name already \
         exists its version is bumped automatically on write; a new skill starts \
         at version 1. Nothing changes this session."
    ))]))
}

struct Handler;
#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle(arguments, ctx).await
    }
}

pub fn entry() -> ToolEntry {
    ToolEntry {
        tool: schema(),
        handler: Box::new(Handler),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockToolContext;

    fn args(name: &str, content: &str, reason: &str) -> Option<JsonObject> {
        match json!({ "name": name, "content": content, "reason": reason }) {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }

    const VALID: &str = "---\nname: greet\ndescription: Say hello nicely\n---\n# Greet\nSay hi.\n";

    #[tokio::test]
    async fn valid_skill_emits_save_effect() {
        let ctx = MockToolContext::new();
        let res = handle(args("greet", VALID, "handy greeting"), &ctx)
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false));
        match &ctx.calls()[0] {
            OutboundToolEffect::SaveSkill(s) => {
                assert_eq!(s.name, "greet");
                assert_eq!(s.reason, "handy greeting");
                assert!(s.content.contains("description: Say hello nicely"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_frontmatter_is_refused_with_precise_error_no_effect() {
        let ctx = MockToolContext::new();
        let err = handle(args("greet", "no frontmatter here\n", "r"), &ctx)
            .await
            .unwrap_err();
        match err {
            ToolError::Validation(msg) => assert!(
                msg.contains("frontmatter") && msg.contains("opening"),
                "got: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert!(ctx.calls().is_empty(), "no effect on validation failure");
    }

    #[tokio::test]
    async fn name_mismatch_is_refused() {
        let ctx = MockToolContext::new();
        let err = handle(args("other-name", VALID, "r"), &ctx)
            .await
            .unwrap_err();
        match err {
            ToolError::Validation(msg) => assert!(msg.contains("does not match"), "got: {msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert!(ctx.calls().is_empty());
    }

    #[tokio::test]
    async fn blank_reason_rejected() {
        let ctx = MockToolContext::new();
        let err = handle(args("greet", VALID, "  "), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.calls().is_empty());
    }

    #[test]
    fn entry_has_expected_name_and_required_fields() {
        let e = entry();
        assert_eq!(e.tool.name.as_ref(), "save_skill");
        let v: serde_json::Value = serde_json::to_value(&*e.tool.input_schema).unwrap();
        assert_eq!(v["required"], json!(["name", "content", "reason"]));
    }
}
