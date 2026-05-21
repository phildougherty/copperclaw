//! Axum router for the GitHub webhook.
//!
//! GitHub dispatches by the `X-GitHub-Event` header, signs the body with
//! HMAC-SHA256 in `X-Hub-Signature-256`, and deduplicates retries via the
//! `X-GitHub-Delivery` UUID.

use crate::events::types::{
    Comment, IssueCommentEvent, IssueRef, IssuesEvent, PullRequestReviewCommentEvent,
};
use crate::signature::verify_signature;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::{DateTime, Utc};
use ironclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};

/// Maximum number of `X-GitHub-Delivery` ids to keep for retry suppression.
pub const DEDUP_CAPACITY: usize = 256;

/// In-memory ring of delivery ids seen, used to suppress GitHub retries.
#[derive(Debug, Default)]
pub struct DeliveryDedup {
    seen: Mutex<VecDeque<String>>,
}

impl DeliveryDedup {
    /// Build an empty dedup ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `id`. Returns true on first sight; false if it was already in
    /// the ring.
    pub async fn observe(&self, id: &str) -> bool {
        let mut guard = self.seen.lock().await;
        if guard.iter().any(|s| s == id) {
            return false;
        }
        if guard.len() == DEDUP_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(id.to_owned());
        true
    }
}

/// Shared state for the GitHub webhook HTTP handler.
#[derive(Clone)]
pub struct GithubEventsState {
    /// Shared secret used to verify `X-Hub-Signature-256`.
    pub webhook_secret: Arc<String>,
    /// Sender to push inbound events back into the host.
    pub inbound_tx: Sender<InboundEvent>,
    /// Delivery-id dedup ring.
    pub dedup: Arc<DeliveryDedup>,
    /// Optional bot login. When set, body text containing `@<login>` flags
    /// `is_mention = true`.
    pub bot_login: Arc<Option<String>>,
    /// Channel-type label attached to emitted events.
    pub channel_type: ChannelType,
}

impl GithubEventsState {
    /// Build new shared state.
    #[must_use]
    pub fn new(
        webhook_secret: impl Into<String>,
        inbound_tx: Sender<InboundEvent>,
        bot_login: Option<String>,
        channel_type: ChannelType,
    ) -> Self {
        Self {
            webhook_secret: Arc::new(webhook_secret.into()),
            inbound_tx,
            dedup: Arc::new(DeliveryDedup::new()),
            bot_login: Arc::new(bot_login),
            channel_type,
        }
    }
}

/// Build the GitHub webhook router. Mounts the handler at the given `path`.
pub fn build_events_router(path: &str, state: GithubEventsState) -> Router {
    Router::new()
        .route(path, post(handle_webhook))
        .with_state(state)
}

async fn handle_webhook(
    State(state): State<GithubEventsState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    if verify_signature(&state.webhook_secret, sig, &body).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let event_kind = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    if event_kind == "ping" {
        // GitHub sends `ping` when a webhook is first installed. Acknowledge.
        return StatusCode::OK.into_response();
    }

    // Retry suppression. Missing delivery header → treat as "not deduped"
    // (we still accept the payload but log it for observability).
    if let Some(delivery_id) = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
    {
        if !state.dedup.observe(delivery_id).await {
            return StatusCode::OK.into_response();
        }
    } else {
        tracing::warn!("github webhook missing X-GitHub-Delivery header");
    }

    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let event = match event_kind.as_str() {
        "issue_comment" => match serde_json::from_value::<IssueCommentEvent>(parsed) {
            Ok(evt) => issue_comment_inbound(&state, &evt),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        "pull_request_review_comment" => {
            match serde_json::from_value::<PullRequestReviewCommentEvent>(parsed) {
                Ok(evt) => pr_review_comment_inbound(&state, &evt),
                Err(_) => return StatusCode::BAD_REQUEST.into_response(),
            }
        }
        "issues" => match serde_json::from_value::<IssuesEvent>(parsed) {
            Ok(evt) => issues_inbound(&state, &evt),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        _ => {
            // Any other event kind — ack and ignore.
            return StatusCode::OK.into_response();
        }
    };

    let Some(event) = event else {
        // Action wasn't one we surface (e.g. `issue_comment` action=edited).
        return StatusCode::OK.into_response();
    };

    if let Err(err) = state.inbound_tx.send(event).await {
        tracing::warn!(error=%err, "github inbound channel closed");
    }
    StatusCode::OK.into_response()
}

fn issue_comment_inbound(
    state: &GithubEventsState,
    evt: &IssueCommentEvent,
) -> Option<InboundEvent> {
    if evt.action != "created" {
        return None;
    }
    let (owner, repo) = evt.repository.split()?;
    let platform_id = format!("{owner}/{repo}#{}", evt.issue.number);
    Some(comment_to_event(state, &platform_id, &evt.comment))
}

fn pr_review_comment_inbound(
    state: &GithubEventsState,
    evt: &PullRequestReviewCommentEvent,
) -> Option<InboundEvent> {
    if evt.action != "created" {
        return None;
    }
    let (owner, repo) = evt.repository.split()?;
    let platform_id = format!("{owner}/{repo}#{}", evt.pull_request.number);
    Some(comment_to_event(state, &platform_id, &evt.comment))
}

fn issues_inbound(state: &GithubEventsState, evt: &IssuesEvent) -> Option<InboundEvent> {
    if evt.action != "opened" {
        return None;
    }
    let (owner, repo) = evt.repository.split()?;
    let platform_id = format!("{owner}/{repo}#{}", evt.issue.number);
    Some(issue_to_event(state, &platform_id, &evt.issue))
}

fn comment_to_event(
    state: &GithubEventsState,
    platform_id: &str,
    comment: &Comment,
) -> InboundEvent {
    let text = comment.body.clone().unwrap_or_default();
    let is_mention = state
        .bot_login
        .as_ref()
        .as_ref()
        .map(|login| body_mentions(&text, login));
    let timestamp = parse_iso8601(comment.created_at.as_deref());
    InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: platform_id.to_owned(),
        thread_id: None,
        message: InboundMessage {
            id: comment.id.to_string(),
            kind: MessageKind::Chat,
            content: json!({"text": text}),
            timestamp,
            is_mention,
            is_group: Some(true),
        },
        reply_to: None,
        sender: Some(SenderIdentity {
            channel_type: state.channel_type.clone(),
            identity: comment.user.login.clone(),
            display_name: Some(comment.user.login.clone()),
        }),
    }
}

fn issue_to_event(
    state: &GithubEventsState,
    platform_id: &str,
    issue: &IssueRef,
) -> InboundEvent {
    let title = issue.title.clone().unwrap_or_default();
    let body = issue.body.clone().unwrap_or_default();
    let text = if body.is_empty() {
        format!("{title}\n\n")
    } else {
        format!("{title}\n\n{body}")
    };
    let is_mention = state
        .bot_login
        .as_ref()
        .as_ref()
        .map(|login| body_mentions(&text, login));
    InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: platform_id.to_owned(),
        thread_id: None,
        message: InboundMessage {
            // Issue events don't carry a comment id; use the issue number.
            id: issue.number.to_string(),
            kind: MessageKind::Chat,
            content: json!({"text": text}),
            timestamp: Utc::now(),
            is_mention,
            is_group: Some(true),
        },
        reply_to: None,
        // GitHub's `issues` payload includes a `user` on the issue itself, but
        // we only deserialize the title/body/number here. The sender is left
        // None on `issues` events; the host can still wire the event by
        // platform_id + channel_type.
        sender: None,
    }
}

/// Whether `text` mentions `@<login>` as a whole word.
pub(crate) fn body_mentions(text: &str, login: &str) -> bool {
    let needle = format!("@{login}");
    let Some(idx) = text.find(&needle) else {
        return false;
    };
    let after = &text[idx + needle.len()..];
    // Reject `@bot-extension` matching `@bot`. The character after the login
    // must not be an alphanumeric / `-` / `_`.
    match after.chars().next() {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '-' || c == '_'),
    }
}

fn parse_iso8601(s: Option<&str>) -> DateTime<Utc> {
    s.and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map_or_else(Utc::now, |dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::compute_signature;
    use axum::body::Body;
    use axum::http::Request;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    const SECRET: &str = "test-secret";

    fn make_state(bot: Option<String>) -> (GithubEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let state = GithubEventsState::new(SECRET, tx, bot, ChannelType::new("github"));
        (state, rx)
    }

    fn signed_request(
        state: &GithubEventsState,
        path: &str,
        event: &str,
        delivery: &str,
        body: &[u8],
    ) -> Request<Body> {
        let sig = compute_signature(&state.webhook_secret, body);
        Request::builder()
            .method("POST")
            .uri(path)
            .header("x-hub-signature-256", sig)
            .header("x-github-event", event)
            .header("x-github-delivery", delivery)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn ping_event_returns_200_with_no_inbound() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let body = b"{}".to_vec();
        let req = signed_request(&state, "/github/webhook", "ping", "d1", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn issue_comment_created_emits_inbound() {
        let (state, mut rx) = make_state(Some("bot".into()));
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"octocat/hello"},
            "issue":{"number":7},
            "comment":{"id":42, "body":"hi @bot there", "user":{"login":"alice"},
                       "created_at":"2024-01-01T12:00:00Z"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d2", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.channel_type.as_str(), "github");
        assert_eq!(evt.platform_id, "octocat/hello#7");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.id, "42");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "hi @bot there");
        assert_eq!(evt.message.is_mention, Some(true));
        assert_eq!(evt.message.is_group, Some(true));
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "alice");
    }

    #[tokio::test]
    async fn issue_comment_edited_is_ignored() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"edited",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":1, "body":"x", "user":{"login":"u"}}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d3", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn pull_request_review_comment_created_uses_pr_number() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "pull_request":{"number":11},
            "comment":{"id":99, "body":"lgtm", "user":{"login":"bob"}}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(
            &state,
            "/github/webhook",
            "pull_request_review_comment",
            "d4",
            &body,
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "o/r#11");
        assert_eq!(evt.message.id, "99");
        assert_eq!(evt.message.content["text"], "lgtm");
        // No bot login configured → is_mention is None.
        assert!(evt.message.is_mention.is_none());
    }

    #[tokio::test]
    async fn issues_opened_concatenates_title_and_body() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"opened",
            "repository":{"full_name":"o/r"},
            "issue":{"number":21, "title":"A bug", "body":"details"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issues", "d5", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "o/r#21");
        assert_eq!(evt.message.id, "21");
        assert_eq!(evt.message.content["text"], "A bug\n\ndetails");
        assert!(evt.sender.is_none());
    }

    #[tokio::test]
    async fn issues_opened_with_null_body_uses_empty() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"opened",
            "repository":{"full_name":"o/r"},
            "issue":{"number":22, "title":"Just a title"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issues", "d6", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["text"], "Just a title\n\n");
    }

    #[tokio::test]
    async fn issues_action_other_than_opened_is_ignored() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"closed",
            "repository":{"full_name":"o/r"},
            "issue":{"number":21, "title":"t"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issues", "d7", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_event_returns_200_no_inbound() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let body = b"{}".to_vec();
        let req = signed_request(&state, "/github/webhook", "star", "d8", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bad_signature_returns_401() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let body = b"{}";
        let req = Request::builder()
            .method("POST")
            .uri("/github/webhook")
            .header("x-hub-signature-256", format!("sha256={}", "00".repeat(32)))
            .header("x-github-event", "ping")
            .header("x-github-delivery", "d-bad")
            .body(Body::from(body.to_vec()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_signature_returns_401() {
        let (_state, _rx) = make_state(None);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let state = GithubEventsState::new(SECRET, tx, None, ChannelType::new("github"));
        let app = build_events_router("/github/webhook", state.clone());
        let req = Request::builder()
            .method("POST")
            .uri("/github/webhook")
            .header("x-github-event", "ping")
            .header("x-github-delivery", "d-bad")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn duplicate_delivery_id_is_suppressed() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":1, "body":"once", "user":{"login":"u"}}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "DUPE", &body);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = signed_request(&state, "/github/webhook", "issue_comment", "DUPE", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _first = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bad_json_returns_400() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let body = b"not json".to_vec();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d9", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_required_field_returns_400() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        // Valid JSON but missing `comment`.
        let body = serde_json::to_vec(&json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1}
        }))
        .unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d10", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_delivery_header_still_processes_event() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":7, "body":"x", "user":{"login":"u"}}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let sig = compute_signature(&state.webhook_secret, &body);
        let req = Request::builder()
            .method("POST")
            .uri("/github/webhook")
            .header("x-hub-signature-256", sig)
            .header("x-github-event", "issue_comment")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "o/r#1");
    }

    #[tokio::test]
    async fn bad_repository_full_name_returns_200_no_inbound() {
        // `full_name` without a slash → split() returns None → action drops.
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"noslash"},
            "issue":{"number":1},
            "comment":{"id":1, "body":"x", "user":{"login":"u"}}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d11", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn body_mentions_detects_at_login() {
        assert!(body_mentions("hi @bot", "bot"));
        assert!(body_mentions("hi @bot.", "bot"));
        assert!(body_mentions("hi @bot there", "bot"));
        assert!(body_mentions("@bot", "bot"));
    }

    #[tokio::test]
    async fn body_mentions_rejects_substring() {
        assert!(!body_mentions("hi @bot-extension", "bot"));
        assert!(!body_mentions("hi @bot_admin", "bot"));
        assert!(!body_mentions("hi @botty", "bot"));
        assert!(!body_mentions("no mention", "bot"));
    }

    #[tokio::test]
    async fn no_bot_login_yields_none_is_mention() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":7, "body":"hi @bot", "user":{"login":"u"}}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d12", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.message.is_mention.is_none());
    }

    #[tokio::test]
    async fn bot_login_without_mention_marks_false() {
        let (state, mut rx) = make_state(Some("bot".into()));
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":7, "body":"just chatting", "user":{"login":"u"}}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d13", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(false));
    }

    #[tokio::test]
    async fn empty_comment_body_is_empty_string() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":7, "body": null, "user":{"login":"u"}}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d14", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["text"], "");
    }

    #[tokio::test]
    async fn comment_created_at_parses_to_utc() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":7, "body":"x", "user":{"login":"u"},
                       "created_at":"2024-01-01T12:00:00Z"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d15", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.timestamp.timestamp(), 1_704_110_400);
    }

    #[tokio::test]
    async fn comment_missing_created_at_falls_back_to_now() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/github/webhook", state.clone());
        let payload = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":7, "body":"x", "user":{"login":"u"}}
        });
        let before = Utc::now().timestamp() - 5;
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/github/webhook", "issue_comment", "d16", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.message.timestamp.timestamp() >= before);
    }

    #[tokio::test]
    async fn delivery_dedup_capacity_drops_oldest() {
        let dedup = DeliveryDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("e{i}")).await);
        }
        assert!(!dedup.observe("e0").await);
        // Push one past capacity → drops the oldest ("e0").
        assert!(dedup.observe("e256").await);
        // "e0" is no longer in the ring.
        assert!(dedup.observe("e0").await);
    }

    #[tokio::test]
    async fn delivery_dedup_new_is_default() {
        let a = DeliveryDedup::new();
        let b = DeliveryDedup::default();
        assert!(a.observe("x").await);
        assert!(b.observe("x").await);
    }

    #[test]
    fn parse_iso8601_returns_now_for_none() {
        let before = Utc::now().timestamp() - 5;
        let dt = parse_iso8601(None);
        assert!(dt.timestamp() >= before);
    }

    #[test]
    fn parse_iso8601_returns_now_for_bad_input() {
        let before = Utc::now().timestamp() - 5;
        let dt = parse_iso8601(Some("not-a-timestamp"));
        assert!(dt.timestamp() >= before);
    }

    #[test]
    fn parse_iso8601_handles_with_offset() {
        let dt = parse_iso8601(Some("2024-01-01T13:00:00+01:00"));
        assert_eq!(dt.timestamp(), 1_704_110_400);
    }

    #[tokio::test]
    async fn state_clone_preserves_dedup_sharing() {
        let (state, _rx) = make_state(None);
        let cloned = state.clone();
        assert!(state.dedup.observe("x").await);
        assert!(!cloned.dedup.observe("x").await);
    }
}
