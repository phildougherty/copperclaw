//! Typed wrappers around [`RpcTransport::call`] for signal-cli's JSON-RPC
//! API. Pure mapping: every method here builds a JSON `params` value, calls
//! the transport, and returns either a typed payload or an
//! [`AdapterError`].
//!
//! Methods covered (per the spec in the channel's `README` of the source
//! file in `lib.rs`):
//!
//! - [`send_text`] — `send` with `recipient` list and `message`.
//! - [`send_to_group`] — `send` with `groupId` and `message`.
//! - [`send_with_attachments`] — `send` with `attachment` paths (in addition
//!   to text/recipient/group).
//! - [`send_edit`] — `sendEditMessage` referencing a previous
//!   `targetSentTimestamp`.
//! - [`send_reaction`] — `sendReaction` with `emoji`, target author + ts.
//! - [`send_typing`] — `sendTyping` with `stop` flag.
//! - [`remote_delete`] — `remoteDelete` with `targetSentTimestamp`.
//! - [`list_groups`] — used by tests to resolve group ids.
//!
//! Every send-shaped method returns the `timestamp` from the response, which
//! signal-cli emits as the platform-side message identifier.

use std::sync::Arc;

use copperclaw_channels_core::AdapterError;
use serde_json::{Value, json};

use crate::rpc::RpcTransport;

/// Destination of an outbound signal-cli `send`. Either a list of recipient
/// e164s for a 1:1 chat, or a base64 group id for a group chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendTarget {
    /// One-to-one recipients (e164 phone numbers).
    Recipients(Vec<String>),
    /// Group id (base64-encoded as signal-cli expects).
    Group(String),
}

impl SendTarget {
    /// Inject `recipient` / `groupId` keys into a `params` object.
    pub fn inject(&self, params: &mut serde_json::Map<String, Value>) {
        match self {
            Self::Recipients(rs) => {
                params.insert("recipient".into(), json!(rs));
            }
            Self::Group(g) => {
                params.insert("groupId".into(), Value::String(g.clone()));
            }
        }
    }
}

/// Extract the `timestamp` (signal-cli's per-message id) from a JSON-RPC
/// `send`-style response. Returns `None` when the field is absent.
pub fn extract_timestamp(value: &Value) -> Option<String> {
    let ts = value.get("timestamp")?;
    if let Some(n) = ts.as_i64() {
        return Some(n.to_string());
    }
    ts.as_str().map(str::to_owned)
}

/// Send a plain text message.
///
/// Builds a `send` request with `recipient` / `groupId` from `target` plus a
/// `message` body. Returns the platform-side message id (`timestamp`).
pub async fn send_text(
    transport: &Arc<dyn RpcTransport>,
    target: &SendTarget,
    message: &str,
) -> Result<Option<String>, AdapterError> {
    let mut params = serde_json::Map::new();
    target.inject(&mut params);
    params.insert("message".into(), Value::String(message.to_owned()));
    let v = transport.call("send", Value::Object(params)).await?;
    Ok(extract_timestamp(&v))
}

/// Send a plain text message to a base64 group id.
///
/// Convenience wrapper around [`send_text`] with [`SendTarget::Group`].
pub async fn send_to_group(
    transport: &Arc<dyn RpcTransport>,
    group_id: &str,
    message: &str,
) -> Result<Option<String>, AdapterError> {
    send_text(
        transport,
        &SendTarget::Group(group_id.to_owned()),
        message,
    )
    .await
}

/// Send a message with attachments. `attachment_paths` is a list of
/// filesystem paths that signal-cli will read at send time.
pub async fn send_with_attachments(
    transport: &Arc<dyn RpcTransport>,
    target: &SendTarget,
    message: &str,
    attachment_paths: &[String],
) -> Result<Option<String>, AdapterError> {
    let mut params = serde_json::Map::new();
    target.inject(&mut params);
    params.insert("message".into(), Value::String(message.to_owned()));
    params.insert("attachment".into(), json!(attachment_paths));
    let v = transport.call("send", Value::Object(params)).await?;
    Ok(extract_timestamp(&v))
}

/// Edit a previously-sent message by `targetSentTimestamp`.
pub async fn send_edit(
    transport: &Arc<dyn RpcTransport>,
    target: &SendTarget,
    target_sent_timestamp: i64,
    new_message: &str,
) -> Result<Option<String>, AdapterError> {
    let mut params = serde_json::Map::new();
    target.inject(&mut params);
    params.insert("targetSentTimestamp".into(), json!(target_sent_timestamp));
    params.insert("message".into(), Value::String(new_message.to_owned()));
    let v = transport
        .call("sendEditMessage", Value::Object(params))
        .await?;
    Ok(extract_timestamp(&v))
}

/// Send a reaction to a previously-sent message.
///
/// `emoji` is passed through verbatim (signal-cli accepts unicode emoji or
/// short codes). `remove = true` (with an empty `emoji`) removes a previous
/// reaction.
pub async fn send_reaction(
    transport: &Arc<dyn RpcTransport>,
    target: &SendTarget,
    emoji: &str,
    target_author: &str,
    target_sent_timestamp: i64,
    remove: bool,
) -> Result<Option<String>, AdapterError> {
    let mut params = serde_json::Map::new();
    target.inject(&mut params);
    params.insert("emoji".into(), Value::String(emoji.to_owned()));
    params.insert("targetAuthor".into(), Value::String(target_author.to_owned()));
    params.insert("targetSentTimestamp".into(), json!(target_sent_timestamp));
    params.insert("remove".into(), Value::Bool(remove));
    let v = transport.call("sendReaction", Value::Object(params)).await?;
    Ok(extract_timestamp(&v))
}

/// Send a typing indicator. `stop = false` indicates typing has started;
/// `stop = true` indicates typing has stopped.
pub async fn send_typing(
    transport: &Arc<dyn RpcTransport>,
    target: &SendTarget,
    stop: bool,
) -> Result<(), AdapterError> {
    let mut params = serde_json::Map::new();
    target.inject(&mut params);
    params.insert("stop".into(), Value::Bool(stop));
    transport.call("sendTyping", Value::Object(params)).await?;
    Ok(())
}

/// Remote-delete a previously-sent message.
pub async fn remote_delete(
    transport: &Arc<dyn RpcTransport>,
    target: &SendTarget,
    target_sent_timestamp: i64,
) -> Result<Option<String>, AdapterError> {
    let mut params = serde_json::Map::new();
    target.inject(&mut params);
    params.insert("targetSentTimestamp".into(), json!(target_sent_timestamp));
    let v = transport.call("remoteDelete", Value::Object(params)).await?;
    Ok(extract_timestamp(&v))
}

/// List groups the account is a member of. Returns the raw JSON-RPC `result`
/// (typically an array of group descriptors).
pub async fn list_groups(transport: &Arc<dyn RpcTransport>) -> Result<Value, AdapterError> {
    transport.call("listGroups", Value::Object(serde_json::Map::new())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{MockTransport, RpcError};
    use serde_json::json;

    fn arc_mock() -> (Arc<dyn RpcTransport>, crate::rpc::MockHandle) {
        let (mock, ctl) = MockTransport::new();
        let arc: Arc<dyn RpcTransport> = Arc::new(mock);
        (arc, ctl)
    }

    #[tokio::test]
    async fn send_text_to_recipient_issues_send_with_recipient_array() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("send", json!({"timestamp": 1_700_000_000_000_i64}))
            .await;
        let id = send_text(
            &t,
            &SendTarget::Recipients(vec!["+15551234".into()]),
            "hi",
        )
        .await
        .unwrap();
        assert_eq!(id.as_deref(), Some("1700000000000"));
        let calls = ctl.calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "send");
        assert_eq!(calls[0].1["recipient"].as_array().unwrap()[0], "+15551234");
        assert_eq!(calls[0].1["message"], "hi");
        assert!(calls[0].1.get("groupId").is_none());
    }

    #[tokio::test]
    async fn send_to_group_uses_group_id_param() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("send", json!({"timestamp": 7})).await;
        let id = send_to_group(&t, "Z3JvdXA=", "hi").await.unwrap();
        assert_eq!(id.as_deref(), Some("7"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].1["groupId"], "Z3JvdXA=");
        assert!(calls[0].1.get("recipient").is_none());
    }

    #[tokio::test]
    async fn send_with_attachments_includes_attachment_paths() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("send", json!({"timestamp": 9})).await;
        let id = send_with_attachments(
            &t,
            &SendTarget::Recipients(vec!["+1".into()]),
            "caption",
            &["/tmp/a.jpg".to_owned(), "/tmp/b.png".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(id.as_deref(), Some("9"));
        let calls = ctl.calls().await;
        let attachments = calls[0].1["attachment"].as_array().unwrap();
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0], "/tmp/a.jpg");
        assert_eq!(attachments[1], "/tmp/b.png");
        assert_eq!(calls[0].1["message"], "caption");
    }

    #[tokio::test]
    async fn send_edit_uses_send_edit_message_method() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("sendEditMessage", json!({"timestamp": 22})).await;
        let id = send_edit(
            &t,
            &SendTarget::Recipients(vec!["+1".into()]),
            1700,
            "updated",
        )
        .await
        .unwrap();
        assert_eq!(id.as_deref(), Some("22"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "sendEditMessage");
        assert_eq!(calls[0].1["targetSentTimestamp"], 1700);
        assert_eq!(calls[0].1["message"], "updated");
    }

    #[tokio::test]
    async fn send_reaction_passes_emoji_and_target() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("sendReaction", json!({"timestamp": 5})).await;
        // Use a placeholder for the emoji so no literal unicode emoji
        // appears in source code (per project style guide). signal-cli
        // accepts any string here.
        let placeholder = "R";
        let id = send_reaction(
            &t,
            &SendTarget::Recipients(vec!["+1".into()]),
            placeholder,
            "+2",
            1700,
            false,
        )
        .await
        .unwrap();
        assert_eq!(id.as_deref(), Some("5"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "sendReaction");
        assert_eq!(calls[0].1["emoji"], placeholder);
        assert_eq!(calls[0].1["targetAuthor"], "+2");
        assert_eq!(calls[0].1["targetSentTimestamp"], 1700);
        assert_eq!(calls[0].1["remove"], false);
    }

    #[tokio::test]
    async fn send_reaction_remove_flag_propagates() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("sendReaction", json!({})).await;
        let _ = send_reaction(
            &t,
            &SendTarget::Group("G==".into()),
            "",
            "+2",
            1700,
            true,
        )
        .await
        .unwrap();
        let calls = ctl.calls().await;
        assert_eq!(calls[0].1["remove"], true);
        assert_eq!(calls[0].1["emoji"], "");
        assert_eq!(calls[0].1["groupId"], "G==");
    }

    #[tokio::test]
    async fn send_typing_passes_stop_flag() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("sendTyping", json!({})).await;
        send_typing(
            &t,
            &SendTarget::Recipients(vec!["+1".into()]),
            false,
        )
        .await
        .unwrap();
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "sendTyping");
        assert_eq!(calls[0].1["stop"], false);
    }

    #[tokio::test]
    async fn send_typing_stop_true() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("sendTyping", json!({})).await;
        send_typing(&t, &SendTarget::Group("G".into()), true)
            .await
            .unwrap();
        let calls = ctl.calls().await;
        assert_eq!(calls[0].1["stop"], true);
        assert_eq!(calls[0].1["groupId"], "G");
    }

    #[tokio::test]
    async fn remote_delete_uses_target_timestamp() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok("remoteDelete", json!({"timestamp": 99})).await;
        let id = remote_delete(
            &t,
            &SendTarget::Recipients(vec!["+1".into()]),
            1700,
        )
        .await
        .unwrap();
        assert_eq!(id.as_deref(), Some("99"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "remoteDelete");
        assert_eq!(calls[0].1["targetSentTimestamp"], 1700);
    }

    #[tokio::test]
    async fn list_groups_returns_result_array() {
        let (t, ctl) = arc_mock();
        ctl.expect_ok(
            "listGroups",
            json!([{"id": "G==", "name": "Group"}]),
        )
        .await;
        let v = list_groups(&t).await.unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "G==");
    }

    #[tokio::test]
    async fn send_text_propagates_rate_error() {
        let (t, ctl) = arc_mock();
        ctl.expect_err(
            "send",
            RpcError {
                code: -3,
                message: "RateLimitException".into(),
                data: None,
            },
        )
        .await;
        let err = send_text(
            &t,
            &SendTarget::Recipients(vec!["+1".into()]),
            "hi",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: None }));
    }

    #[tokio::test]
    async fn send_text_propagates_auth_error() {
        let (t, ctl) = arc_mock();
        ctl.expect_err(
            "send",
            RpcError {
                code: -1,
                message: "AuthorizationFailedException".into(),
                data: None,
            },
        )
        .await;
        let err = send_text(
            &t,
            &SendTarget::Recipients(vec!["+1".into()]),
            "hi",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn send_text_propagates_other_error_as_bad_request() {
        let (t, ctl) = arc_mock();
        ctl.expect_err(
            "send",
            RpcError {
                code: -32601,
                message: "Method not found".into(),
                data: None,
            },
        )
        .await;
        let err = send_text(
            &t,
            &SendTarget::Recipients(vec!["+1".into()]),
            "hi",
        )
        .await
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("-32601")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn extract_timestamp_from_integer_field() {
        let v = json!({"timestamp": 1_700_000_000_000_i64});
        assert_eq!(extract_timestamp(&v).as_deref(), Some("1700000000000"));
    }

    #[tokio::test]
    async fn extract_timestamp_from_string_field() {
        let v = json!({"timestamp": "1700000000000"});
        assert_eq!(extract_timestamp(&v).as_deref(), Some("1700000000000"));
    }

    #[tokio::test]
    async fn extract_timestamp_returns_none_when_absent() {
        assert!(extract_timestamp(&json!({})).is_none());
    }

    #[tokio::test]
    async fn extract_timestamp_returns_none_for_unsupported_type() {
        assert!(extract_timestamp(&json!({"timestamp": [1]})).is_none());
    }

    #[test]
    fn send_target_inject_recipients() {
        let mut params = serde_json::Map::new();
        SendTarget::Recipients(vec!["+1".into(), "+2".into()]).inject(&mut params);
        let recipients = params["recipient"].as_array().unwrap();
        assert_eq!(recipients.len(), 2);
    }

    #[test]
    fn send_target_inject_group() {
        let mut params = serde_json::Map::new();
        SendTarget::Group("G".into()).inject(&mut params);
        assert_eq!(params["groupId"], "G");
    }

    #[test]
    fn send_target_clone_and_eq() {
        let a = SendTarget::Recipients(vec!["+1".into()]);
        let b = a.clone();
        assert_eq!(a, b);
        let g1 = SendTarget::Group("g".into());
        let g2 = SendTarget::Group("g".into());
        assert_eq!(g1, g2);
    }

    #[test]
    fn send_target_debug() {
        let a = SendTarget::Recipients(vec!["+1".into()]);
        let s = format!("{a:?}");
        assert!(s.contains("Recipients"));
    }
}
