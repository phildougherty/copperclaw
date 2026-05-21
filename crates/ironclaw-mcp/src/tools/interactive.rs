//! Interactive tools: `ask_user_question`, `send_card`.

use crate::context::Recipient;
use crate::error::ToolError;
use crate::tools::core::RecipientInput;

fn validate_to(input: Option<RecipientInput>) -> Result<Option<Recipient>, ToolError> {
    crate::tools::core::validate_to(input)
}

pub mod ask_user_question {
    //! `ask_user_question`: present a titled multiple-choice question.

    use super::{validate_to, RecipientInput};
    use crate::context::{AskUserQuestionSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ack_to_result, make_tool, parse_args, ToolEntry, ToolHandler};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        title: String,
        options: Vec<String>,
        #[serde(default)]
        to: Option<RecipientInput>,
    }

    pub fn schema() -> Tool {
        make_tool(
            "ask_user_question",
            "Ask the user a titled question with a fixed list of allowed answers.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["title", "options"],
                "properties": {
                    "title": { "type": "string", "minLength": 1 },
                    "options": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "minItems": 1
                    },
                    "to": { "type": ["string", "object", "null"] }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.title.trim().is_empty() {
            return Err(ToolError::Validation("`title` must be non-empty".into()));
        }
        if input.options.is_empty() {
            return Err(ToolError::Validation(
                "`options` must have at least one entry".into(),
            ));
        }
        if input.options.iter().any(|o| o.trim().is_empty()) {
            return Err(ToolError::Validation(
                "`options` entries must be non-empty".into(),
            ));
        }
        let to = validate_to(input.to)?;
        let spec = AskUserQuestionSpec {
            title: input.title,
            options: input.options,
            to,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::AskUserQuestion(spec))
            .await?;
        Ok(ack_to_result(&ack))
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
}

pub mod send_card {
    //! `send_card`: send a structured card. The card body is opaque to this
    //! crate — the channel adapter validates the payload.

    use super::{validate_to, RecipientInput};
    use crate::context::{OutboundToolEffect, SendCardSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ack_to_result, make_tool, parse_args, ToolEntry, ToolHandler};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        #[serde(default)]
        to: Option<RecipientInput>,
        card: serde_json::Value,
    }

    pub fn schema() -> Tool {
        make_tool(
            "send_card",
            "Send a structured card payload understood by the destination channel adapter.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["card"],
                "properties": {
                    "to": { "type": ["string", "object", "null"] },
                    "card": { "type": "object" }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if !input.card.is_object() {
            return Err(ToolError::Validation("`card` must be an object".into()));
        }
        let to = validate_to(input.to)?;
        let spec = SendCardSpec {
            to,
            card: input.card,
        };
        let ack = ctx.emit_outbound(OutboundToolEffect::SendCard(spec)).await?;
        Ok(ack_to_result(&ack))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{MockToolContext, OutboundToolEffect};
    use rmcp::model::JsonObject;
    use serde_json::Value;

    fn args_from(value: Value) -> Option<JsonObject> {
        match value {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    #[tokio::test]
    async fn ask_user_question_happy() {
        let ctx = MockToolContext::new();
        ask_user_question::handle(
            args_from(serde_json::json!({"title": "Pick", "options": ["a", "b"]})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::AskUserQuestion(s) => {
                assert_eq!(s.title, "Pick");
                assert_eq!(s.options, vec!["a", "b"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_user_question_empty_title() {
        let ctx = MockToolContext::new();
        let err = ask_user_question::handle(
            args_from(serde_json::json!({"title": "  ", "options": ["a"]})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn ask_user_question_empty_options() {
        let ctx = MockToolContext::new();
        let err = ask_user_question::handle(
            args_from(serde_json::json!({"title": "Pick", "options": []})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn ask_user_question_blank_option() {
        let ctx = MockToolContext::new();
        let err = ask_user_question::handle(
            args_from(serde_json::json!({"title": "Pick", "options": ["a", " "]})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn send_card_happy() {
        let ctx = MockToolContext::new();
        send_card::handle(
            args_from(serde_json::json!({"card": {"title": "Hi"}})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::SendCard(s) => {
                assert_eq!(s.card["title"], "Hi");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_card_validation_not_object() {
        let ctx = MockToolContext::new();
        let err = send_card::handle(args_from(serde_json::json!({"card": [1, 2]})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn send_card_missing_card() {
        let ctx = MockToolContext::new();
        let err = send_card::handle(args_from(serde_json::json!({})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn interactive_schemas_have_required() {
        let s = ask_user_question::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["title", "options"]));

        let s = send_card::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["card"]));
    }
}
