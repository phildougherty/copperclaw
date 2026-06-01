//! Typed wrappers around [`RpcTransport::call`] for the
//! `deltachat-rpc-server` methods we care about.
//!
//! Each function builds the appropriate positional `params` array, issues
//! the RPC, and decodes the result into a typed shape. Errors from the
//! transport are surfaced as [`AdapterError`].

use crate::rpc::RpcTransport;
use copperclaw_channels_core::AdapterError;
use serde::Deserialize;
use serde_json::{Value, json};

/// Result of [`get_message`] — the fields we use from the deltachat
/// `MessageObject` reply.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MessageView {
    /// Server message id.
    pub id: i64,
    /// Chat the message lives in.
    pub chat_id: i64,
    /// Contact id of the sender.
    pub from_id: i64,
    /// Message body text (may be empty).
    #[serde(default)]
    pub text: String,
    /// `true` for system info messages (e.g. "Alice joined").
    #[serde(default)]
    pub is_info: bool,
    /// View type the deltachat core assigned (`"Text"`, `"Image"`, etc.).
    #[serde(default, rename = "view_type")]
    pub view_type: String,
    /// Optional path to a downloaded file on disk.
    #[serde(default)]
    pub file: Option<String>,
    /// Optional filename of the attachment.
    #[serde(default)]
    pub filename: Option<String>,
    /// Optional MIME type (e.g. `"image/jpeg"`) the server inferred.
    #[serde(default)]
    pub file_mime: Option<String>,
    /// Optional file size in bytes as reported by the server.
    #[serde(default)]
    pub file_bytes: Option<u64>,
    /// Download state: `"Done"`, `"Available"`, `"InProgress"`, `"Failure"`.
    /// Anything other than `"Done"` means the body isn't on disk yet; the
    /// adapter will issue `download_full_msg` to materialise it.
    #[serde(default)]
    pub download_state: Option<String>,
    /// Unix timestamp (seconds) of the message.
    #[serde(default)]
    pub timestamp: i64,
    /// Display name of the sender, when known.
    #[serde(default)]
    pub sender_name: Option<String>,
}

/// Result of [`get_basic_chat_info`] — only the fields the adapter needs.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChatInfo {
    /// Chat id.
    pub id: i64,
    /// Chat type per `deltachat-rpc-server`:
    /// `1` = single (1-1), `2` = group, `3` = mailinglist,
    /// `4` = broadcast.
    #[serde(default)]
    pub chat_type: i64,
    /// Chat display name.
    #[serde(default)]
    pub name: String,
}

impl ChatInfo {
    /// Whether this chat counts as a group conversation
    /// (i.e. not a 1-1 DM).
    pub fn is_group(&self) -> bool {
        !matches!(self.chat_type, 1)
    }
}

/// `add_account` — returns the newly created account id.
pub async fn add_account(transport: &dyn RpcTransport) -> Result<u64, AdapterError> {
    let v = transport.call("add_account", json!([])).await?;
    v.as_u64()
        .ok_or_else(|| AdapterError::BadRequest(format!("add_account returned non-number: {v}")))
}

/// `get_all_account_ids` — returns the configured account ids.
pub async fn get_all_account_ids(transport: &dyn RpcTransport) -> Result<Vec<u64>, AdapterError> {
    let v = transport.call("get_all_account_ids", json!([])).await?;
    let arr = v.as_array().ok_or_else(|| {
        AdapterError::BadRequest(format!("get_all_account_ids returned non-array: {v}"))
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let n = v.as_u64().ok_or_else(|| {
            AdapterError::BadRequest("account id must be a non-negative integer".into())
        })?;
        out.push(n);
    }
    Ok(out)
}

/// `send_msg` — send a message and return its server id.
///
/// `payload` is the deltachat `MessageData` object; callers should at
/// minimum set `"text"`. Optional fields supported by the server include
/// `"file"`, `"filename"`, and `"quoted_message_id"`.
pub async fn send_msg(
    transport: &dyn RpcTransport,
    account_id: u64,
    chat_id: i64,
    payload: Value,
) -> Result<i64, AdapterError> {
    let v = transport
        .call("send_msg", json!([account_id, chat_id, payload]))
        .await?;
    v.as_i64()
        .ok_or_else(|| AdapterError::BadRequest(format!("send_msg returned non-integer id: {v}")))
}

/// `send_reaction` — react to a message with one or more emoji.
pub async fn send_reaction(
    transport: &dyn RpcTransport,
    account_id: u64,
    msg_id: i64,
    emojis: &[String],
) -> Result<i64, AdapterError> {
    let v = transport
        .call("send_reaction", json!([account_id, msg_id, emojis]))
        .await?;
    // send_reaction returns the id of the reaction message; some server
    // versions return null. Treat null as a no-id success.
    if v.is_null() {
        return Ok(0);
    }
    v.as_i64().ok_or_else(|| {
        AdapterError::BadRequest(format!("send_reaction returned non-integer id: {v}"))
    })
}

/// `delete_messages` — remove the listed messages for the calling user.
pub async fn delete_messages(
    transport: &dyn RpcTransport,
    account_id: u64,
    msg_ids: &[i64],
) -> Result<(), AdapterError> {
    let _ = transport
        .call("delete_messages", json!([account_id, msg_ids]))
        .await?;
    Ok(())
}

/// `get_message` — fetch a single message.
pub async fn get_message(
    transport: &dyn RpcTransport,
    account_id: u64,
    msg_id: i64,
) -> Result<MessageView, AdapterError> {
    let v = transport
        .call("get_message", json!([account_id, msg_id]))
        .await?;
    serde_json::from_value(v)
        .map_err(|e| AdapterError::BadRequest(format!("get_message decode failed: {e}")))
}

/// `get_basic_chat_info` — fetch a chat's metadata.
pub async fn get_basic_chat_info(
    transport: &dyn RpcTransport,
    account_id: u64,
    chat_id: i64,
) -> Result<ChatInfo, AdapterError> {
    let v = transport
        .call("get_basic_chat_info", json!([account_id, chat_id]))
        .await?;
    serde_json::from_value(v)
        .map_err(|e| AdapterError::BadRequest(format!("get_basic_chat_info decode failed: {e}")))
}

/// `get_next_event` — block until the next event from the server.
pub async fn get_next_event(transport: &dyn RpcTransport) -> Result<Value, AdapterError> {
    transport.next_event().await
}

/// `download_full_msg` — request that the server materialise the body of a
/// partially-downloaded message. Returns nothing on success; the server
/// later emits a `MsgsChanged` event when the file is available.
pub async fn download_full_msg(
    transport: &dyn RpcTransport,
    account_id: u64,
    msg_id: i64,
) -> Result<(), AdapterError> {
    let _ = transport
        .call("download_full_msg", json!([account_id, msg_id]))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{MockResponse, MockTransport};
    use serde_json::json;

    #[tokio::test]
    async fn add_account_returns_id() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("add_account", json!(5)))
            .await;
        let id = add_account(&m).await.unwrap();
        assert_eq!(id, 5);
        let calls = m.observed().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "add_account");
        assert_eq!(calls[0].params, json!([]));
    }

    #[tokio::test]
    async fn add_account_rejects_non_number_payload() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("add_account", json!("nope")))
            .await;
        let err = add_account(&m).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn get_all_account_ids_decodes_array() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("get_all_account_ids", json!([1, 2, 3])))
            .await;
        let ids = get_all_account_ids(&m).await.unwrap();
        assert_eq!(ids, vec![1, 2, 3]);
        let calls = m.observed().await;
        assert_eq!(calls[0].method, "get_all_account_ids");
    }

    #[tokio::test]
    async fn get_all_account_ids_rejects_non_array() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("get_all_account_ids", json!(1)))
            .await;
        let err = get_all_account_ids(&m).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn get_all_account_ids_rejects_non_integer_entry() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("get_all_account_ids", json!(["a"])))
            .await;
        let err = get_all_account_ids(&m).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn send_msg_returns_message_id_and_params_shape_is_correct() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("send_msg", json!(101)))
            .await;
        let id = send_msg(&m, 1, 42, json!({"text": "hi"})).await.unwrap();
        assert_eq!(id, 101);
        let calls = m.observed().await;
        assert_eq!(calls[0].method, "send_msg");
        assert_eq!(calls[0].params, json!([1, 42, {"text": "hi"}]));
    }

    #[tokio::test]
    async fn send_msg_rejects_non_integer_result() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("send_msg", json!("oops")))
            .await;
        let err = send_msg(&m, 1, 1, json!({})).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn send_reaction_passes_emojis_array() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("send_reaction", json!(7)))
            .await;
        let id = send_reaction(&m, 1, 50, &["+1".to_owned()]).await.unwrap();
        assert_eq!(id, 7);
        let calls = m.observed().await;
        assert_eq!(calls[0].method, "send_reaction");
        assert_eq!(calls[0].params, json!([1, 50, ["+1"]]));
    }

    #[tokio::test]
    async fn send_reaction_null_result_returns_zero() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("send_reaction", Value::Null))
            .await;
        let id = send_reaction(&m, 1, 50, &["+1".to_owned()]).await.unwrap();
        assert_eq!(id, 0);
    }

    #[tokio::test]
    async fn send_reaction_rejects_non_integer_result() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("send_reaction", json!("nope")))
            .await;
        let err = send_reaction(&m, 1, 50, &["+1".to_owned()])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn delete_messages_invokes_with_account_and_ids() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("delete_messages", Value::Null))
            .await;
        delete_messages(&m, 1, &[10, 20]).await.unwrap();
        let calls = m.observed().await;
        assert_eq!(calls[0].method, "delete_messages");
        assert_eq!(calls[0].params, json!([1, [10, 20]]));
    }

    #[tokio::test]
    async fn delete_messages_propagates_transport_error() {
        let m = MockTransport::new();
        m.push_response(MockResponse::err(
            "delete_messages",
            AdapterError::Transport("dead".into()),
        ))
        .await;
        let err = delete_messages(&m, 1, &[1]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn get_message_decodes_full_shape() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok(
            "get_message",
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "hello", "is_info": false,
                "view_type": "Text", "timestamp": 1_700_000_000,
                "sender_name": "Alice"
            }),
        ))
        .await;
        let view = get_message(&m, 1, 100).await.unwrap();
        assert_eq!(view.id, 100);
        assert_eq!(view.chat_id, 42);
        assert_eq!(view.from_id, 7);
        assert_eq!(view.text, "hello");
        assert!(!view.is_info);
        assert_eq!(view.view_type, "Text");
        assert_eq!(view.timestamp, 1_700_000_000);
        assert_eq!(view.sender_name.as_deref(), Some("Alice"));
        let calls = m.observed().await;
        assert_eq!(calls[0].method, "get_message");
        assert_eq!(calls[0].params, json!([1, 100]));
    }

    #[tokio::test]
    async fn get_message_optional_attachment_fields_decode() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok(
            "get_message",
            json!({
                "id": 1, "chat_id": 1, "from_id": 2,
                "view_type": "File",
                "file": "/tmp/x", "filename": "x.bin",
                "timestamp": 1
            }),
        ))
        .await;
        let view = get_message(&m, 1, 1).await.unwrap();
        assert_eq!(view.file.as_deref(), Some("/tmp/x"));
        assert_eq!(view.filename.as_deref(), Some("x.bin"));
        assert_eq!(view.text, "");
    }

    #[tokio::test]
    async fn get_message_decode_failure_is_bad_request() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("get_message", json!("not an object")))
            .await;
        let err = get_message(&m, 1, 1).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn get_basic_chat_info_decodes_group_type() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok(
            "get_basic_chat_info",
            json!({"id": 42, "chat_type": 2, "name": "Team"}),
        ))
        .await;
        let info = get_basic_chat_info(&m, 1, 42).await.unwrap();
        assert_eq!(info.id, 42);
        assert_eq!(info.chat_type, 2);
        assert_eq!(info.name, "Team");
        assert!(info.is_group());
    }

    #[tokio::test]
    async fn get_basic_chat_info_decode_failure_is_bad_request() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("get_basic_chat_info", json!("nope")))
            .await;
        let err = get_basic_chat_info(&m, 1, 1).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn get_next_event_returns_event_payload() {
        let m = MockTransport::new();
        m.push_event(json!({"kind": "Info", "msg": "hi"})).await;
        let v = get_next_event(&m).await.unwrap();
        assert_eq!(v["kind"], "Info");
    }

    #[tokio::test]
    async fn get_next_event_propagates_transport_error() {
        let m = MockTransport::new();
        m.push_event_error(AdapterError::Transport("nope".into()))
            .await;
        let err = get_next_event(&m).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn chat_info_is_group_branches() {
        let single = ChatInfo {
            id: 1,
            chat_type: 1,
            name: "Alice".into(),
        };
        assert!(!single.is_group());
        let group = ChatInfo {
            id: 1,
            chat_type: 2,
            name: "Team".into(),
        };
        assert!(group.is_group());
        let ml = ChatInfo {
            id: 1,
            chat_type: 3,
            name: "List".into(),
        };
        assert!(ml.is_group());
        let bc = ChatInfo {
            id: 1,
            chat_type: 4,
            name: "Broadcast".into(),
        };
        assert!(bc.is_group());
    }

    #[test]
    fn message_view_default_fields_are_optional() {
        let raw = json!({"id": 1, "chat_id": 2, "from_id": 3});
        let view: MessageView = serde_json::from_value(raw).unwrap();
        assert_eq!(view.text, "");
        assert!(!view.is_info);
        assert!(view.file.is_none());
        assert!(view.filename.is_none());
        assert_eq!(view.timestamp, 0);
    }

    #[test]
    fn message_view_clone_and_debug() {
        let v = MessageView {
            id: 1,
            chat_id: 1,
            from_id: 1,
            text: "x".into(),
            is_info: false,
            view_type: "Text".into(),
            file: None,
            filename: None,
            file_mime: None,
            file_bytes: None,
            download_state: Some("Done".into()),
            timestamp: 0,
            sender_name: None,
        };
        let _ = v.clone();
        assert!(format!("{v:?}").contains("MessageView"));
    }

    #[test]
    fn chat_info_clone_and_debug() {
        let c = ChatInfo {
            id: 1,
            chat_type: 1,
            name: "x".into(),
        };
        let _ = c.clone();
        assert!(format!("{c:?}").contains("ChatInfo"));
    }
}
