//! GitHub [`ChannelAdapter`] implementation.

use crate::api::GithubApi;
use crate::emoji::to_reaction_slug;
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, OutboundMessage};
use serde_json::Value;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// GitHub channel adapter. See crate-level docs.
pub struct GithubAdapter {
    channel_type: ChannelType,
    api: GithubApi,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for GithubAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl GithubAdapter {
    /// Construct with an already-built API client. Used by the factory and by
    /// tests that drive the adapter directly.
    #[must_use]
    pub fn new(channel_type: ChannelType, api: GithubApi) -> Self {
        Self {
            channel_type,
            api,
            server_handle: Mutex::new(None),
        }
    }

    /// Attach the join handle of the spawned axum server so the adapter can
    /// abort it on shutdown.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("github adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background webhook server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("github adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    /// Borrow the underlying API client (mostly useful for tests).
    #[must_use]
    pub fn api(&self) -> &GithubApi {
        &self.api
    }
}

/// Parse the `{owner}/{repo}#{number}` shape into its three pieces.
///
/// Returns [`AdapterError::BadRequest`] if any piece is missing or `number`
/// fails to parse as a positive integer.
pub fn parse_platform_id(platform_id: &str) -> Result<(String, String, u64), AdapterError> {
    let (repo_full, number_str) = platform_id.split_once('#').ok_or_else(|| {
        AdapterError::BadRequest(format!(
            "github platform_id must be `<owner>/<repo>#<number>`, got `{platform_id}`"
        ))
    })?;
    let (owner, repo) = repo_full.split_once('/').ok_or_else(|| {
        AdapterError::BadRequest(format!(
            "github platform_id repository part must be `<owner>/<repo>`, got `{repo_full}`"
        ))
    })?;
    if owner.is_empty() || repo.is_empty() {
        return Err(AdapterError::BadRequest(format!(
            "github platform_id repository part has empty owner or repo: `{repo_full}`"
        )));
    }
    let number: u64 = number_str.parse().map_err(|_| {
        AdapterError::BadRequest(format!(
            "github platform_id number must be a positive integer, got `{number_str}`"
        ))
    })?;
    Ok((owner.to_owned(), repo.to_owned(), number))
}

#[async_trait]
impl ChannelAdapter for GithubAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        false
    }

    async fn set_typing(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // GitHub has no typing indicator. No-op.
        Ok(())
    }

    async fn subscribe(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // Webhook ingress observes all configured repos automatically; no
        // per-conversation subscribe is required.
        Ok(())
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let (owner, repo, number) = parse_platform_id(platform_id)?;

        // System actions: edit / reaction.
        if let Some(action) = message.content.get("action").and_then(Value::as_str) {
            match action {
                "edit" => {
                    let comment_id = required_target_id(&message.content)?;
                    let text = message
                        .content
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let resp = self
                        .api
                        .edit_comment(&owner, &repo, comment_id, text)
                        .await?;
                    return Ok(Some(resp.id.to_string()));
                }
                "reaction" => {
                    let comment_id = required_target_id(&message.content)?;
                    let emoji = message
                        .content
                        .get("emoji")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            AdapterError::BadRequest(
                                "github reaction action requires `emoji` field".into(),
                            )
                        })?;
                    let slug = to_reaction_slug(emoji).ok_or_else(|| {
                        AdapterError::BadRequest(format!(
                            "github does not support reaction emoji `{emoji}`"
                        ))
                    })?;
                    self.api
                        .add_reaction(&owner, &repo, comment_id, slug)
                        .await?;
                    return Ok(None);
                }
                other => {
                    return Err(AdapterError::BadRequest(format!(
                        "github does not support action `{other}`"
                    )));
                }
            }
        }

        // Plain comment. We don't ship file attachments in v1 — GitHub doesn't
        // accept binary uploads on issue comments. If the agent sent files,
        // surface a typed error rather than silently dropping them.
        if !message.files.is_empty() {
            return Err(AdapterError::Unsupported(
                "github channel does not support file attachments on comments".into(),
            ));
        }
        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let resp = self.api.post_comment(&owner, &repo, number, &text).await?;
        Ok(Some(resp.id.to_string()))
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // GitHub has no DM concept at the API surface we model.
        Ok(None)
    }
}

fn required_target_id(content: &Value) -> Result<i64, AdapterError> {
    // Accept either `target_id` (the comment id we want to act on) or the
    // host-side convention `target_seq` (which the host translates upstream
    // into the platform message id before this point). Numeric form is
    // required; we don't attempt to look anything up.
    let raw = content
        .get("target_id")
        .or_else(|| content.get("target_seq"))
        .ok_or_else(|| {
            AdapterError::BadRequest(
                "github edit/reaction action requires `target_id` or `target_seq`".into(),
            )
        })?;
    match raw {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| AdapterError::BadRequest("target id must fit in i64".into())),
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| AdapterError::BadRequest("target id string must parse as i64".into())),
        _ => Err(AdapterError::BadRequest(
            "target id must be a number or numeric string".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::{MessageKind, OutboundFile};
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> GithubAdapter {
        GithubAdapter::new(
            ChannelType::new("github"),
            GithubApi::new(server.uri(), "ghp-test"),
        )
    }

    fn text(msg: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": msg}),
            files: vec![],
        }
    }

    #[tokio::test]
    async fn channel_type_and_thread_support() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert_eq!(adapter.channel_type().as_str(), "github");
        assert!(!adapter.supports_threads());
    }

    #[tokio::test]
    async fn deliver_posts_comment_and_returns_id_as_string() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/octocat/hello/issues/7/comments"))
            .and(body_json(json!({"body":"hi"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 42})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("octocat/hello#7", None, &text("hi"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn deliver_ignores_thread_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 5})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("o/r#7", Some("ignored"), &text("hi"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("5"));
    }

    #[tokio::test]
    async fn deliver_edit_action_uses_patch() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/issues/comments/42"))
            .and(body_json(json!({"body":"edited"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": 42, "html_url":"u"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit", "target_id": 42, "text":"edited"}),
            files: vec![],
        };
        let id = adapter.deliver("o/r#7", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn deliver_edit_action_accepts_numeric_string_target() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/issues/comments/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 42})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit", "target_id":"42", "text":"x"}),
            files: vec![],
        };
        adapter.deliver("o/r#7", None, &msg).await.unwrap();
    }

    #[tokio::test]
    async fn deliver_edit_accepts_target_seq_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/issues/comments/100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 100})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit", "target_seq": 100, "text":"x"}),
            files: vec![],
        };
        adapter.deliver("o/r#7", None, &msg).await.unwrap();
    }

    #[tokio::test]
    async fn deliver_reaction_action_posts_slug() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/comments/42/reactions"))
            .and(body_json(json!({"content":"+1"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 99})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction", "target_id": 42, "emoji":"thumbsup"}),
            files: vec![],
        };
        let id = adapter.deliver("o/r#7", None, &msg).await.unwrap();
        // Reaction does not yield a platform message id.
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn deliver_reaction_with_unknown_emoji_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction", "target_id": 42, "emoji":"unknown-emoji"}),
            files: vec![],
        };
        match adapter.deliver("o/r#7", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("unknown-emoji")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_missing_emoji_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction", "target_id": 42}),
            files: vec![],
        };
        match adapter.deliver("o/r#7", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("emoji")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_missing_target_id_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit", "text":"x"}),
            files: vec![],
        };
        match adapter.deliver("o/r#7", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("target_id")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_non_numeric_target_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit", "target_id": "abc", "text":"x"}),
            files: vec![],
        };
        match adapter.deliver("o/r#7", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_object_target_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit", "target_id": {}, "text":"x"}),
            files: vec![],
        };
        match adapter.deliver("o/r#7", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"weird", "target_id": 1}),
            files: vec![],
        };
        match adapter.deliver("o/r#7", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("weird")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_files_is_unsupported() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":"hi"}),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: b"data".to_vec(),
            }],
        };
        match adapter.deliver("o/r#7", None, &msg).await {
            Err(AdapterError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_malformed_platform_id_no_hash_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        match adapter.deliver("o/r-no-hash", None, &text("x")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_malformed_platform_id_no_slash_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        match adapter.deliver("noslash#7", None, &text("x")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_malformed_platform_id_empty_owner_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        match adapter.deliver("/r#7", None, &text("x")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_malformed_platform_id_bad_number_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        match adapter.deliver("o/r#abc", None, &text("x")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_surfaces_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("o/r#7", None, &text("x")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rate_limited_via_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "5"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("o/r#7", None, &text("x")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(5)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_text_defaults_to_empty_when_missing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .and(body_json(json!({"body":""})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![],
        };
        let id = adapter.deliver("o/r#7", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn set_typing_is_noop_ok() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.set_typing("o/r#7", None).await.unwrap();
        adapter.set_typing("o/r#7", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_is_noop_ok() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.subscribe("o/r#7", None).await.unwrap();
        adapter.subscribe("o/r#7", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(adapter.open_dm("u").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn api_accessor_returns_inner() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(format!("{:?}", adapter.api()).contains("ghp-test"));
    }

    #[tokio::test]
    async fn server_handle_shutdown_is_idempotent() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        adapter.set_server_handle(task);
        adapter.shutdown_server();
        adapter.shutdown_server();
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let s = format!("{adapter:?}");
        assert!(s.contains("GithubAdapter"));
        assert!(s.contains("github"));
    }

    #[test]
    fn parse_platform_id_round_trips() {
        let (o, r, n) = parse_platform_id("octocat/hello#7").unwrap();
        assert_eq!(o, "octocat");
        assert_eq!(r, "hello");
        assert_eq!(n, 7);
    }

    #[test]
    fn parse_platform_id_requires_hash() {
        let err = parse_platform_id("octocat/hello").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_platform_id_requires_slash() {
        let err = parse_platform_id("noslash#1").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_platform_id_rejects_negative_number() {
        let err = parse_platform_id("o/r#-1").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_platform_id_rejects_zero_ok() {
        // Zero parses; GitHub itself will return 404 on POST.
        let (_, _, n) = parse_platform_id("o/r#0").unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn parse_platform_id_rejects_empty_repo() {
        let err = parse_platform_id("o/#7").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn required_target_id_rejects_bool() {
        let v = json!({"target_id": true});
        let err = required_target_id(&v).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn required_target_id_handles_number() {
        let v = json!({"target_id": 7});
        assert_eq!(required_target_id(&v).unwrap(), 7);
    }

    #[test]
    fn required_target_id_handles_numeric_string() {
        let v = json!({"target_id": "7"});
        assert_eq!(required_target_id(&v).unwrap(), 7);
    }
}
