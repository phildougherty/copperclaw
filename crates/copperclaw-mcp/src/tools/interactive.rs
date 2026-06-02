//! Interactive tools: `ask_user_question`, `send_card`.

use crate::context::Recipient;
use crate::error::ToolError;
use crate::tools::core::RecipientInput;

fn validate_to(input: Option<RecipientInput>) -> Result<Option<Recipient>, ToolError> {
    crate::tools::core::validate_to(input)
}

pub mod ask_user_question {
    //! `ask_user_question`: present a titled multiple-choice question.

    use super::{RecipientInput, validate_to};
    use crate::context::{AskUserQuestionSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
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
            "Ask the user a titled question with a fixed list of allowed answers. \
             The user's pick comes back as an inbound message on a later turn (you \
             don't block). In almost all cases OMIT `to` — the question is sent to the \
             user/channel you're already talking to. Only set `to` to redirect the \
             question somewhere else.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["title", "options"],
                "properties": {
                    "title": { "type": "string", "minLength": 1, "description": "Question text shown above the choices." },
                    "options": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "minItems": 1,
                        "description": "The allowed answers; the user picks exactly one."
                    },
                    "to": {
                        "type": ["string", "object", "null"],
                        "description": "OPTIONAL — leave it out to ask the user on the current channel (this is what you want almost every time). Only to redirect elsewhere: pass a fully-qualified channel-id STRING like \"telegram:chat-123\", OR an object with an explicit kind — {\"kind\":\"user\",\"id\":\"<user-id>\"}, {\"kind\":\"channel\",\"id\":\"<channel-id>\"}, or {\"kind\":\"agent\",\"session_id\":\"<session-id>\"}. Any other object shape is rejected."
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
    //! `send_card`: send a canonical, portable card that every channel
    //! adapter knows how to render.
    //!
    //! The card schema lives in `copperclaw-channels-core::Card`. Channels
    //! with native card support (Telegram inline keyboards, Slack Block
    //! Kit, Discord embeds, Google Chat cards v2, etc.) render the
    //! structure directly via their `deliver_card` override. Channels
    //! without native support fall back automatically to a deterministic
    //! text rendering via the trait-level default — so calling
    //! `send_card` from any agent on any channel produces a usable
    //! result.

    use super::{RecipientInput, validate_to};
    use crate::context::{OutboundToolEffect, SendCardSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use copperclaw_channels_core::Card;
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        #[serde(default)]
        to: Option<RecipientInput>,
        /// Canonical card payload — parsed directly into [`Card`] so the
        /// MCP boundary catches malformed input before the runner ever
        /// sees it.
        card: Card,
    }

    pub fn schema() -> Tool {
        make_tool(
            "send_card",
            // The description is what the model sees — keep it grounded
            // in the portability story so the agent reaches for cards
            // when it has structured content, regardless of channel.
            "Portable card schema — works on every channel. Channels with native \
             card support (Telegram, Slack, etc.) render the structure; channels \
             without it fall back to formatted text. Provide title/body/fields/\
             buttons/image_url. Buttons each have either `value` (for callback) \
             or `url` (for link).",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["card"],
                "properties": {
                    "to": { "type": ["string", "object", "null"] },
                    "card": {
                        "type": "object",
                        "additionalProperties": false,
                        "description": "Canonical card. At least one of title, body, fields, or image_url must be present.",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "Headline. Rendered bold on text-only channels; embed title on Discord; etc."
                            },
                            "body": {
                                "type": "string",
                                "description": "Body paragraph. Markdown or plain text — adapters handle channel-specific escaping."
                            },
                            "fields": {
                                "type": "array",
                                "description": "Key/value rows. Rendered as a small table where the channel supports it.",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["label", "value"],
                                    "properties": {
                                        "label": { "type": "string" },
                                        "value": { "type": "string" },
                                        "inline": {
                                            "type": "boolean",
                                            "description": "Hint: render side-by-side with the next field when the channel supports it."
                                        }
                                    }
                                }
                            },
                            "buttons": {
                                "type": "array",
                                "description": "Interactive buttons. Each button must set EITHER `value` (callback payload sent back as an inbound chat message) OR `url` (opens a link). Set exactly one — both is rejected.",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["label"],
                                    "properties": {
                                        "label": { "type": "string" },
                                        "value": {
                                            "type": "string",
                                            "description": "Callback payload. ≤ 64 bytes."
                                        },
                                        "url": {
                                            "type": "string",
                                            "description": "http(s) URL to open."
                                        },
                                        "style": {
                                            "type": "string",
                                            "description": "primary | danger | secondary. Adapters that don't support styles ignore this.",
                                            "enum": ["primary", "danger", "secondary"]
                                        }
                                    }
                                }
                            },
                            "image_url": {
                                "type": "string",
                                "description": "http(s) image URL. Rendered inline where supported."
                            }
                        }
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
        // Run the canonical schema validator at the MCP boundary so the
        // model gets a precise error message and the runner never
        // touches an invalid card.
        input
            .card
            .validate()
            .map_err(|e| ToolError::Validation(e.to_string()))?;
        let to = validate_to(input.to)?;
        let spec = SendCardSpec {
            to,
            card: input.card,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::SendCard(spec))
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
    async fn ask_user_question_to_string_is_channel() {
        // A bare channel-id string is the convenience form.
        let ctx = MockToolContext::new();
        ask_user_question::handle(
            args_from(serde_json::json!({
                "title": "Pick", "options": ["a"], "to": "telegram:chat-9"
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::AskUserQuestion(s) => assert_eq!(
                s.to,
                Some(crate::context::Recipient::Channel {
                    id: "telegram:chat-9".into()
                })
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_user_question_to_tagged_object_ok() {
        // The explicit `kind`-tagged object form.
        let ctx = MockToolContext::new();
        ask_user_question::handle(
            args_from(serde_json::json!({
                "title": "Pick", "options": ["a"], "to": {"kind": "user", "id": "u1"}
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::AskUserQuestion(s) => {
                assert_eq!(
                    s.to,
                    Some(crate::context::Recipient::User { id: "u1".into() })
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_user_question_to_object_without_kind_is_rejected() {
        // The #1 agent mistake: an object without an explicit `kind`. This
        // must fail (rather than silently mis-route) — the schema/skill now
        // steer the model to omit `to` or use the tagged form.
        let ctx = MockToolContext::new();
        let err = ask_user_question::handle(
            args_from(serde_json::json!({
                "title": "Pick", "options": ["a"], "to": {"user": "u1"}
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        // Parse-layer rejection (untagged enum miss) surfaces as a tool error.
        let _ = err;
        assert!(ctx.calls().is_empty(), "no outbound emitted for a bad `to`");
    }

    #[test]
    fn ask_user_question_schema_documents_to() {
        // Guard the guidance: the `to` field must tell the model it's
        // optional (omit) and how to shape it. Lost descriptions are how the
        // original "every form of `to` is rejected" flailing happened.
        let s = ask_user_question::schema();
        let v: Value = serde_json::to_value(&*s.input_schema).unwrap();
        let to_desc = v["properties"]["to"]["description"].as_str().unwrap_or("");
        assert!(to_desc.contains("OPTIONAL"), "to.description: {to_desc}");
        assert!(to_desc.contains("kind"), "to.description: {to_desc}");
    }

    #[tokio::test]
    async fn send_card_happy() {
        let ctx = MockToolContext::new();
        // Full canonical card — title, body, one field, one button, one image.
        send_card::handle(
            args_from(serde_json::json!({
                "card": {
                    "title": "Order #42",
                    "body": "Confirm?",
                    "fields": [{"label": "Item", "value": "Espresso"}],
                    "buttons": [{"label": "Confirm", "value": "confirm:42"}],
                    "image_url": "https://example.com/x.png"
                }
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::SendCard(s) => {
                assert_eq!(s.card.title.as_deref(), Some("Order #42"));
                assert_eq!(s.card.body.as_deref(), Some("Confirm?"));
                assert_eq!(s.card.fields.len(), 1);
                assert_eq!(s.card.fields[0].label, "Item");
                assert_eq!(s.card.buttons.len(), 1);
                assert_eq!(s.card.buttons[0].value.as_deref(), Some("confirm:42"));
                assert_eq!(
                    s.card.image_url.as_deref(),
                    Some("https://example.com/x.png")
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_card_rejects_empty_card() {
        // An empty `{}` card has no title / body / fields / image — the
        // canonical validator rejects it as `CardError::Empty`.
        let ctx = MockToolContext::new();
        let err = send_card::handle(args_from(serde_json::json!({"card": {}})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn send_card_rejects_button_without_target() {
        // A button with neither `value` nor `url` is rejected.
        let ctx = MockToolContext::new();
        let err = send_card::handle(
            args_from(serde_json::json!({
                "card": {
                    "title": "Hi",
                    "buttons": [{"label": "Click"}]
                }
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        let msg = match err {
            ToolError::Validation(m) => m,
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(msg.contains("value") || msg.contains("url"), "got: {msg}");
    }

    #[tokio::test]
    async fn send_card_rejects_button_with_bad_url_scheme() {
        // `javascript:` URLs are rejected by the canonical validator —
        // proves the schema check actually runs at the MCP boundary.
        let ctx = MockToolContext::new();
        let err = send_card::handle(
            args_from(serde_json::json!({
                "card": {
                    "title": "Hi",
                    "buttons": [{"label": "Click", "url": "javascript:alert(1)"}]
                }
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn send_card_rejects_card_not_object() {
        // Card field must deserialise into a Card struct — passing an
        // array fails at the serde stage.
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
        // The card sub-schema enumerates the canonical fields so the model
        // gets type-level hints without reading the docstring.
        let props = &v["properties"]["card"]["properties"];
        assert!(props.get("title").is_some());
        assert!(props.get("body").is_some());
        assert!(props.get("fields").is_some());
        assert!(props.get("buttons").is_some());
        assert!(props.get("image_url").is_some());
    }
}
