//! Core messaging tools: `send_message`, `send_file`, `edit_message`,
//! `add_reaction`.
//!
//! Each tool is its own submodule so the schema, handler, and tests stay
//! co-located.

use serde::Deserialize;

use crate::context::Recipient;
use crate::error::ToolError;

/// Shared `to` field. Accepted forms:
/// - `"telegram:chat-123"` — bare channel id string (convenience).
/// - `{ "kind": "channel", "id": "..." }` / `{ "kind": "agent", ... }` /
///   `{ "kind": "user", ... }` — explicit `Recipient`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum RecipientInput {
    /// Bare string form: treated as a fully-qualified channel id.
    Channel(String),
    /// Explicit tagged form.
    Tagged(Recipient),
}

impl RecipientInput {
    pub(crate) fn into_recipient(self) -> Recipient {
        match self {
            Self::Channel(id) => Recipient::Channel { id },
            Self::Tagged(r) => r,
        }
    }
}

/// Validate and normalise an optional `to` argument.
pub(crate) fn validate_to(input: Option<RecipientInput>) -> Result<Option<Recipient>, ToolError> {
    let Some(input) = input else {
        return Ok(None);
    };
    let r = input.into_recipient();
    let id_empty = match &r {
        Recipient::Channel { id } | Recipient::User { id } => id.trim().is_empty(),
        Recipient::Agent { session_id } => session_id.trim().is_empty(),
    };
    if id_empty {
        return Err(ToolError::Validation("`to` id is empty".into()));
    }
    Ok(Some(r))
}

pub mod send_message {
    //! `send_message`: emit a plain text message to the originating channel
    //! or an explicit recipient.

    use super::{validate_to, RecipientInput};
    use crate::context::{OutboundToolEffect, SendMessageSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ack_to_result, make_tool, parse_args, ToolEntry, ToolHandler};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    /// JSON-RPC arguments for `send_message`.
    #[derive(Debug, Deserialize)]
    struct Input {
        #[serde(default)]
        to: Option<RecipientInput>,
        text: String,
    }

    /// Build the rmcp `Tool` descriptor.
    pub fn schema() -> Tool {
        make_tool(
            "send_message",
            "Send a plain-text message. Omit `to` to reply on the originating channel.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["text"],
                "properties": {
                    "to": { "type": ["string", "object", "null"] },
                    "text": { "type": "string", "minLength": 1 }
                }
            }),
        )
    }

    /// Run the tool.
    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.text.trim().is_empty() {
            return Err(ToolError::Validation("`text` must be non-empty".into()));
        }
        let to = validate_to(input.to)?;
        let spec = SendMessageSpec {
            to,
            text: input.text,
        };
        let ack = ctx.emit_outbound(OutboundToolEffect::SendMessage(spec)).await?;
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

    /// `ToolEntry` for registration.
    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

pub mod send_file {
    //! `send_file`: emit a file (optionally with caption).

    use super::{validate_to, RecipientInput};
    use crate::context::{OutboundToolEffect, SendFileSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ack_to_result, make_tool, parse_args, ToolEntry, ToolHandler};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        #[serde(default)]
        to: Option<RecipientInput>,
        filename: String,
        #[serde(with = "crate::context::bytes_b64")]
        data: Vec<u8>,
        #[serde(default)]
        text: Option<String>,
    }

    pub fn schema() -> Tool {
        make_tool(
            "send_file",
            "Send a file (base64-encoded bytes). `text` is an optional caption.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["filename", "data"],
                "properties": {
                    "to": { "type": ["string", "object", "null"] },
                    "filename": { "type": "string", "minLength": 1 },
                    "data": { "type": "string", "contentEncoding": "base64" },
                    "text": { "type": ["string", "null"] }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.filename.trim().is_empty() {
            return Err(ToolError::Validation("`filename` must be non-empty".into()));
        }
        if input.data.is_empty() {
            return Err(ToolError::Validation("`data` must be non-empty".into()));
        }
        let to = validate_to(input.to)?;
        let spec = SendFileSpec {
            to,
            filename: input.filename,
            data: input.data,
            text: input.text,
        };
        let ack = ctx.emit_outbound(OutboundToolEffect::SendFile(spec)).await?;
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

pub mod edit_message {
    //! `edit_message`: replace the text of a previously sent message,
    //! identified by its outbound `seq`.

    use crate::context::{EditMessageSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ack_to_result, make_tool, parse_args, ToolEntry, ToolHandler};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        message_id: i64,
        text: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "edit_message",
            "Edit the text of a previously sent message by its int sequence id.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["message_id", "text"],
                "properties": {
                    "message_id": { "type": "integer" },
                    "text": { "type": "string", "minLength": 1 }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.message_id <= 0 {
            return Err(ToolError::Validation(
                "`message_id` must be positive".into(),
            ));
        }
        if input.text.trim().is_empty() {
            return Err(ToolError::Validation("`text` must be non-empty".into()));
        }
        let spec = EditMessageSpec {
            message_seq: input.message_id,
            text: input.text,
        };
        let ack = ctx.emit_outbound(OutboundToolEffect::EditMessage(spec)).await?;
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

pub mod add_reaction {
    //! `add_reaction`: add a reaction to a previously sent message.

    use crate::context::{AddReactionSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ack_to_result, make_tool, parse_args, ToolEntry, ToolHandler};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        message_id: i64,
        emoji: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "add_reaction",
            "Add an emoji reaction to a previously sent message by its int sequence id.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["message_id", "emoji"],
                "properties": {
                    "message_id": { "type": "integer" },
                    "emoji": { "type": "string", "minLength": 1 }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.message_id <= 0 {
            return Err(ToolError::Validation(
                "`message_id` must be positive".into(),
            ));
        }
        if input.emoji.trim().is_empty() {
            return Err(ToolError::Validation("`emoji` must be non-empty".into()));
        }
        let spec = AddReactionSpec {
            message_seq: input.message_id,
            emoji: input.emoji,
        };
        let ack = ctx.emit_outbound(OutboundToolEffect::AddReaction(spec)).await?;
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
    use crate::context::{
        MockToolContext, OutboundToolEffect, SendMessageSpec, ToolEffectAck,
    };
    use rmcp::model::JsonObject;
    use serde_json::Value;

    fn args_from(value: Value) -> Option<JsonObject> {
        match value {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    #[tokio::test]
    async fn send_message_happy() {
        let ctx = MockToolContext::new();
        ctx.set_next_ack(ToolEffectAck::Message { seq: 1 });
        let res = send_message::handle(
            args_from(serde_json::json!({"text": "hi"})),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(res.is_error, Some(false));
        let calls = ctx.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            &calls[0],
            OutboundToolEffect::SendMessage(s) if s.text == "hi" && s.to.is_none()
        ));
    }

    #[tokio::test]
    async fn send_message_with_string_to() {
        let ctx = MockToolContext::new();
        send_message::handle(
            args_from(serde_json::json!({"to": "telegram:chat-1", "text": "hi"})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::SendMessage(SendMessageSpec {
                to: Some(Recipient::Channel { id }),
                ..
            }) => assert_eq!(id, "telegram:chat-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_message_with_tagged_to() {
        let ctx = MockToolContext::new();
        send_message::handle(
            args_from(
                serde_json::json!({"to": {"kind": "agent", "session_id": "sess_2"}, "text": "hi"}),
            ),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::SendMessage(SendMessageSpec {
                to: Some(Recipient::Agent { session_id }),
                ..
            }) => assert_eq!(session_id, "sess_2"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_message_validation_empty_text() {
        let ctx = MockToolContext::new();
        let err = send_message::handle(args_from(serde_json::json!({"text": "  "})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert_eq!(ctx.call_count(), 0);
    }

    #[tokio::test]
    async fn send_message_validation_missing_text() {
        let ctx = MockToolContext::new();
        let err = send_message::handle(args_from(serde_json::json!({})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn send_message_validation_empty_to_id() {
        let ctx = MockToolContext::new();
        let err = send_message::handle(
            args_from(serde_json::json!({"to": "  ", "text": "hi"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn send_message_context_failure_propagates() {
        let ctx = MockToolContext::new();
        ctx.fail_next_emit(ToolError::Context("db".into()));
        let err = send_message::handle(args_from(serde_json::json!({"text": "hi"})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Context(_)));
    }

    #[tokio::test]
    async fn send_file_happy() {
        let ctx = MockToolContext::new();
        send_file::handle(
            args_from(serde_json::json!({"filename": "x.txt", "data": "aGVsbG8="})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::SendFile(s) => {
                assert_eq!(s.filename, "x.txt");
                assert_eq!(s.data, b"hello");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_file_validation_empty_data() {
        let ctx = MockToolContext::new();
        let err = send_file::handle(
            args_from(serde_json::json!({"filename": "x", "data": ""})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn send_file_validation_empty_filename() {
        let ctx = MockToolContext::new();
        let err = send_file::handle(
            args_from(serde_json::json!({"filename": "", "data": "aGk="})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn send_file_bad_base64_is_validation() {
        let ctx = MockToolContext::new();
        let err = send_file::handle(
            args_from(serde_json::json!({"filename": "x", "data": "not_base64!"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn edit_message_happy() {
        let ctx = MockToolContext::new();
        edit_message::handle(
            args_from(serde_json::json!({"message_id": 7, "text": "edited"})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::EditMessage(s) => {
                assert_eq!(s.message_seq, 7);
                assert_eq!(s.text, "edited");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn edit_message_validation_nonpositive_id() {
        let ctx = MockToolContext::new();
        let err = edit_message::handle(
            args_from(serde_json::json!({"message_id": 0, "text": "x"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn edit_message_validation_empty_text() {
        let ctx = MockToolContext::new();
        let err = edit_message::handle(
            args_from(serde_json::json!({"message_id": 7, "text": ""})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn add_reaction_happy() {
        let ctx = MockToolContext::new();
        add_reaction::handle(
            args_from(serde_json::json!({"message_id": 7, "emoji": "thumbsup"})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::AddReaction(s) => {
                assert_eq!(s.message_seq, 7);
                assert_eq!(s.emoji, "thumbsup");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_reaction_validation_nonpositive() {
        let ctx = MockToolContext::new();
        let err = add_reaction::handle(
            args_from(serde_json::json!({"message_id": -1, "emoji": "x"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn add_reaction_validation_empty_emoji() {
        let ctx = MockToolContext::new();
        let err = add_reaction::handle(
            args_from(serde_json::json!({"message_id": 1, "emoji": "  "})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn core_schemas_declare_required_fields() {
        let s = send_message::schema();
        let schema: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["text"]));

        let s = send_file::schema();
        let schema: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["filename", "data"]));

        let s = edit_message::schema();
        let schema: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(
            schema["required"],
            serde_json::json!(["message_id", "text"])
        );

        let s = add_reaction::schema();
        let schema: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(
            schema["required"],
            serde_json::json!(["message_id", "emoji"])
        );
    }
}

