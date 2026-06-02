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

    use super::{RecipientInput, validate_to};
    use crate::context::{OutboundToolEffect, SendMessageSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
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
        let ack = ctx
            .emit_outbound(OutboundToolEffect::SendMessage(spec))
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

    use super::{RecipientInput, validate_to};
    use crate::context::{OutboundToolEffect, SendFileSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    /// Hard ceiling on bytes a single `send_file` will read from disk.
    /// Channel adapters cap separately (Telegram is 50 MB) — this is a
    /// runner-side guard so a malicious model can't tie up the host
    /// trying to upload a 4 GB log file. Pick comfortably above the
    /// largest payload any sane attachment uses.
    const SEND_FILE_PATH_MAX_BYTES: usize = 32 * 1024 * 1024; // 32 MB

    #[derive(Debug, Deserialize)]
    struct Input {
        #[serde(default)]
        to: Option<RecipientInput>,
        #[serde(default)]
        filename: Option<String>,
        /// **Preferred** for any file already on disk inside the
        /// container (typical case: the agent just wrote it with
        /// `write_file` or a build step generated it). The handler
        /// reads the file directly — no base64 dance in the model's
        /// output. Mutually exclusive with `data`.
        #[serde(default)]
        path: Option<String>,
        /// Use only for bytes the model has actually generated
        /// in-memory and cannot save to disk first. Base64-encoded
        /// strings over a few KB are an anti-pattern here — they
        /// blow past `max_tokens` mid-tool-call. Mutually exclusive
        /// with `path`.
        #[serde(default, with = "crate::context::bytes_b64_optional")]
        data: Option<Vec<u8>>,
        #[serde(default)]
        text: Option<String>,
    }

    pub fn schema() -> Tool {
        make_tool(
            "send_file",
            "Send a file. Provide EITHER `path` (preferred; the host reads the file from disk \
             — use this for anything you wrote with `write_file` or a build artifact) OR \
             `data` (base64-encoded bytes; only use for bytes generated in-memory that you \
             can't save first). DO NOT base64-encode a file on disk and pass it as `data` — \
             that overflows the model's max_tokens. `filename` is required when using `data`, \
             optional when using `path` (defaults to the basename). `text` is an optional \
             caption.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "to":       { "type": ["string", "object", "null"] },
                    "path":     { "type": ["string", "null"], "description": "Filesystem path inside the container. Preferred for files on disk." },
                    "filename": { "type": ["string", "null"], "description": "Display name for the recipient. Required with `data`; defaults to path basename when using `path`." },
                    "data":     { "type": ["string", "null"], "contentEncoding": "base64", "description": "Base64 bytes. Avoid for files on disk — use `path` instead." },
                    "text":     { "type": ["string", "null"] }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let to = validate_to(input.to)?;

        // Resolve to (filename, bytes). Exactly one of `path` / `data`
        // must be supplied. The path branch is the recommended way
        // for any file the agent already has on disk; the data
        // branch is the legacy in-memory-bytes path.
        let (filename, data) = match (input.path.as_deref(), input.data) {
            (Some(_), Some(_)) => {
                return Err(ToolError::Validation(
                    "send_file: provide either `path` or `data`, not both".into(),
                ));
            }
            (None, None) => {
                return Err(ToolError::Validation(
                    "send_file: must provide either `path` (preferred) or `data`".into(),
                ));
            }
            (Some(path), None) => {
                let metadata = std::fs::metadata(path).map_err(|e| {
                    ToolError::Validation(format!("send_file: stat `{path}` failed: {e}"))
                })?;
                let len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
                if len > SEND_FILE_PATH_MAX_BYTES {
                    return Err(ToolError::Validation(format!(
                        "send_file: `{path}` is {len} bytes; max is {SEND_FILE_PATH_MAX_BYTES}"
                    )));
                }
                let bytes = std::fs::read(path).map_err(|e| {
                    ToolError::Validation(format!("send_file: read `{path}` failed: {e}"))
                })?;
                let fname = input.filename.unwrap_or_else(|| {
                    std::path::Path::new(path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("file")
                        .to_string()
                });
                (fname, bytes)
            }
            (None, Some(bytes)) => {
                let fname = input.filename.ok_or_else(|| {
                    ToolError::Validation(
                        "send_file: `filename` is required when using `data`".into(),
                    )
                })?;
                (fname, bytes)
            }
        };

        if filename.trim().is_empty() {
            return Err(ToolError::Validation("`filename` must be non-empty".into()));
        }
        if data.is_empty() {
            return Err(ToolError::Validation("file is empty (zero bytes)".into()));
        }

        let spec = SendFileSpec {
            to,
            filename,
            data,
            text: input.text,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::SendFile(spec))
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

pub mod edit_message {
    //! `edit_message`: replace the text of a previously sent message,
    //! identified by its outbound `seq`.

    use crate::context::{EditMessageSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
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
        let ack = ctx
            .emit_outbound(OutboundToolEffect::EditMessage(spec))
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

pub mod add_reaction {
    //! `add_reaction`: add a reaction to a previously sent message.

    use crate::context::{AddReactionSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
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
        let ack = ctx
            .emit_outbound(OutboundToolEffect::AddReaction(spec))
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
    use crate::context::{MockToolContext, OutboundToolEffect, SendMessageSpec, ToolEffectAck};
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
        let res = send_message::handle(args_from(serde_json::json!({"text": "hi"})), &ctx)
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
        // send_file uses runtime cross-validation (exactly one of
        // `path` or `data`) instead of schema `required` — the
        // either-or rule isn't easily expressible in JSON Schema's
        // top-level `required` array.
        assert_eq!(schema["required"], serde_json::Value::Null);
        let props = schema["properties"].as_object().expect("properties object");
        for key in ["path", "data", "filename", "text", "to"] {
            assert!(
                props.contains_key(key),
                "send_file schema should advertise property {key}: got {props:?}"
            );
        }

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
