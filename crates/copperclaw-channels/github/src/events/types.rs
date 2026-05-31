//! GitHub webhook payload types.
//!
//! We only deserialize the fields we use. The catch-all `Other` variant lets
//! payloads we ignore parse without errors.

use serde::Deserialize;

/// Top-level webhook payload. The event kind is carried in the
/// `X-GitHub-Event` header rather than the body itself, so we dispatch on
/// that and then deserialize into one of these shapes.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueCommentEvent {
    /// Webhook action — only `created` produces an inbound event.
    pub action: String,
    /// Repository the event came from.
    pub repository: Repository,
    /// Issue or PR the comment is on. Both kinds of comments end up here.
    pub issue: IssueRef,
    /// The comment itself.
    pub comment: Comment,
}

/// Pull-request review-comment webhook payload.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestReviewCommentEvent {
    /// Webhook action — only `created` produces an inbound event.
    pub action: String,
    /// Repository the comment lives in.
    pub repository: Repository,
    /// PR the comment is on.
    pub pull_request: PullRequestRef,
    /// The review comment itself.
    pub comment: Comment,
}

/// `issues` webhook payload (issue opened, edited, closed, …).
#[derive(Debug, Clone, Deserialize)]
pub struct IssuesEvent {
    /// Webhook action — only `opened` produces an inbound event in v1.
    pub action: String,
    /// Repository the issue lives in.
    pub repository: Repository,
    /// The issue.
    pub issue: IssueRef,
}

/// Repository portion of every webhook payload.
#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    /// `<owner>/<repo>` full name string.
    pub full_name: String,
}

impl Repository {
    /// Split `full_name` into `(owner, repo)`. Returns `None` if the slash is
    /// missing — the router treats that as a bad payload.
    #[must_use]
    pub fn split(&self) -> Option<(&str, &str)> {
        let (a, b) = self.full_name.split_once('/')?;
        if a.is_empty() || b.is_empty() {
            return None;
        }
        Some((a, b))
    }
}

/// Issue or PR reference (same shape — GitHub returns this for both).
#[derive(Debug, Clone, Deserialize)]
pub struct IssueRef {
    /// Issue number within the repo. For issue-comment events that fired on a
    /// PR, this is still the PR's number (GitHub overloads `issue`).
    pub number: u64,
    /// Issue title. Used by the `issues` event to render the inbound body.
    #[serde(default)]
    pub title: Option<String>,
    /// Issue body. Often `null`; treat as empty when missing.
    #[serde(default)]
    pub body: Option<String>,
}

/// Pull-request reference (review-comment payloads carry the PR explicitly).
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestRef {
    /// PR number within the repo.
    pub number: u64,
}

/// Comment payload (issue comments + PR review comments are the same shape).
#[derive(Debug, Clone, Deserialize)]
pub struct Comment {
    /// Comment id (used for edit / reaction operations).
    pub id: i64,
    /// Comment body. `None` indicates an empty comment.
    #[serde(default)]
    pub body: Option<String>,
    /// User who authored the comment.
    pub user: User,
    /// ISO-8601 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Minimal user shape — we only need the login for `is_mention` and the
/// `SenderIdentity`.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    /// GitHub login string (e.g. `octocat`).
    pub login: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn issue_comment_event_parses() {
        let v = json!({
            "action": "created",
            "repository": {"full_name": "octocat/hello"},
            "issue": {"number": 7, "title": "T", "body": "B"},
            "comment": {
                "id": 42,
                "body": "hi @bot",
                "user": {"login": "alice"},
                "created_at": "2024-01-01T00:00:00Z"
            }
        });
        let evt: IssueCommentEvent = serde_json::from_value(v).unwrap();
        assert_eq!(evt.action, "created");
        assert_eq!(evt.repository.full_name, "octocat/hello");
        assert_eq!(evt.issue.number, 7);
        assert_eq!(evt.comment.id, 42);
        assert_eq!(evt.comment.body.as_deref(), Some("hi @bot"));
        assert_eq!(evt.comment.user.login, "alice");
        assert_eq!(evt.comment.created_at.as_deref(), Some("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn pull_request_review_comment_event_parses() {
        let v = json!({
            "action": "created",
            "repository": {"full_name": "octocat/hello"},
            "pull_request": {"number": 11},
            "comment": {
                "id": 99,
                "body": "lgtm",
                "user": {"login": "bob"}
            }
        });
        let evt: PullRequestReviewCommentEvent = serde_json::from_value(v).unwrap();
        assert_eq!(evt.action, "created");
        assert_eq!(evt.pull_request.number, 11);
        assert_eq!(evt.comment.id, 99);
    }

    #[test]
    fn issues_event_parses() {
        let v = json!({
            "action": "opened",
            "repository": {"full_name": "octocat/hello"},
            "issue": {"number": 21, "title": "bug", "body": "details"}
        });
        let evt: IssuesEvent = serde_json::from_value(v).unwrap();
        assert_eq!(evt.action, "opened");
        assert_eq!(evt.issue.number, 21);
        assert_eq!(evt.issue.title.as_deref(), Some("bug"));
        assert_eq!(evt.issue.body.as_deref(), Some("details"));
    }

    #[test]
    fn issue_body_missing_is_none() {
        let v = json!({
            "action":"opened",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1, "title":"t"}
        });
        let evt: IssuesEvent = serde_json::from_value(v).unwrap();
        assert!(evt.issue.body.is_none());
    }

    #[test]
    fn issue_body_null_is_none() {
        let v = json!({
            "action":"opened",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1, "title":"t", "body": null}
        });
        let evt: IssuesEvent = serde_json::from_value(v).unwrap();
        assert!(evt.issue.body.is_none());
    }

    #[test]
    fn comment_body_null_is_none() {
        let v = json!({
            "action":"created",
            "repository":{"full_name":"o/r"},
            "issue":{"number":1},
            "comment":{"id":1, "body": null, "user":{"login":"u"}}
        });
        let evt: IssueCommentEvent = serde_json::from_value(v).unwrap();
        assert!(evt.comment.body.is_none());
    }

    #[test]
    fn repository_split_returns_pair() {
        let r = Repository {
            full_name: "octocat/hello".into(),
        };
        assert_eq!(r.split(), Some(("octocat", "hello")));
    }

    #[test]
    fn repository_split_rejects_no_slash() {
        let r = Repository {
            full_name: "no-slash".into(),
        };
        assert_eq!(r.split(), None);
    }

    #[test]
    fn repository_split_rejects_empty_owner() {
        let r = Repository {
            full_name: "/repo".into(),
        };
        assert_eq!(r.split(), None);
    }

    #[test]
    fn repository_split_rejects_empty_repo() {
        let r = Repository {
            full_name: "owner/".into(),
        };
        assert_eq!(r.split(), None);
    }

    #[test]
    fn debug_format_present() {
        let r = Repository {
            full_name: "o/r".into(),
        };
        assert!(format!("{r:?}").contains("o/r"));
    }

    #[test]
    fn clone_works() {
        let c = Comment {
            id: 1,
            body: Some("x".into()),
            user: User {
                login: "u".into(),
            },
            created_at: None,
        };
        let c2 = c.clone();
        assert_eq!(c.id, c2.id);
        assert_eq!(c.user.login, c2.user.login);
    }
}
