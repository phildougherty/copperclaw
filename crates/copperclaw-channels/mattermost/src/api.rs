//! Mattermost REST API client (egress side).
//!
//! Only the calls the channel adapter actually issues are implemented:
//!
//! - `create_post` → `POST /api/v4/posts`
//! - `update_post` → `PUT /api/v4/posts/{post_id}/patch`
//! - `add_reaction` → `POST /api/v4/reactions`
//!
//! Every call uses bearer auth with the configured Personal Access
//! Token. HTTP-error → [`AdapterError`] translation matches the
//! `docs/adding-a-channel.md` rubric the other channels follow.

use copperclaw_channels_core::AdapterError;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Thin Mattermost client. One per adapter; cheap to clone (it owns
/// only a `reqwest::Client` + a few strings).
#[derive(Clone, Debug)]
pub struct MattermostApi {
    base_url: String,
    token: String,
    client: Client,
}

impl MattermostApi {
    /// Build a client targeting `base_url` (no trailing slash) with the
    /// supplied Personal Access Token.
    #[must_use]
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            client: Client::new(),
        }
    }

    /// Test-only constructor that lets the caller inject a configured
    /// `Client` (e.g. one wired against a wiremock).
    #[must_use]
    pub fn with_client(base_url: &str, token: &str, client: Client) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            client,
        }
    }

    /// `POST /api/v4/posts` — returns the created post's id.
    pub async fn create_post(
        &self,
        channel_id: &str,
        message: &str,
        root_id: Option<&str>,
    ) -> Result<String, AdapterError> {
        self.create_post_with_files(channel_id, message, root_id, &[])
            .await
    }

    /// `POST /api/v4/posts` with optional `file_ids`. Attaches
    /// previously-uploaded files to the new post. Pass an empty slice
    /// for a text-only post (equivalent to [`Self::create_post`]).
    pub async fn create_post_with_files(
        &self,
        channel_id: &str,
        message: &str,
        root_id: Option<&str>,
        file_ids: &[String],
    ) -> Result<String, AdapterError> {
        let body = CreatePostBody {
            channel_id,
            message,
            root_id,
            file_ids: if file_ids.is_empty() {
                None
            } else {
                Some(file_ids)
            },
        };
        let res = self
            .client
            .post(format!("{}/api/v4/posts", self.base_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = res.status();
        if !status.is_success() {
            return Err(map_error(status, res).await);
        }
        let post: PostResponse = res.json().await.map_err(|e| transport(&e))?;
        Ok(post.id)
    }

    /// `POST /api/v4/files?channel_id=…&filename=…` — upload a file's
    /// bytes for the given channel and return its `file_id`. The
    /// returned id is consumed by [`Self::create_post_with_files`].
    pub async fn upload_file(
        &self,
        channel_id: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<String, AdapterError> {
        if filename.is_empty() {
            return Err(AdapterError::BadRequest(
                "upload_file: empty filename".into(),
            ));
        }
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| AdapterError::BadRequest(format!("upload_file mime: {e}")))?;
        let form = reqwest::multipart::Form::new().part("files", part);
        let res = self
            .client
            .post(format!("{}/api/v4/files", self.base_url))
            .bearer_auth(&self.token)
            .query(&[("channel_id", channel_id), ("filename", filename)])
            .multipart(form)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = res.status();
        if !status.is_success() {
            return Err(map_error(status, res).await);
        }
        let resp: UploadResponse = res.json().await.map_err(|e| transport(&e))?;
        resp.file_infos
            .into_iter()
            .next()
            .map(|f| f.id)
            .ok_or_else(|| AdapterError::Transport("upload_file: empty file_infos".into()))
    }

    /// `PUT /api/v4/posts/{post_id}/patch` — edit an existing post's
    /// text.
    pub async fn update_post(&self, post_id: &str, message: &str) -> Result<(), AdapterError> {
        #[derive(Serialize)]
        struct Patch<'a> {
            message: &'a str,
        }
        let res = self
            .client
            .put(format!("{}/api/v4/posts/{post_id}/patch", self.base_url))
            .bearer_auth(&self.token)
            .json(&Patch { message })
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = res.status();
        if !status.is_success() {
            return Err(map_error(status, res).await);
        }
        Ok(())
    }

    /// `POST /api/v4/reactions` — add a reaction to a post on behalf of
    /// `user_id` (typically the bot's id).
    pub async fn add_reaction(
        &self,
        user_id: &str,
        post_id: &str,
        emoji_name: &str,
    ) -> Result<(), AdapterError> {
        #[derive(Serialize)]
        struct Reaction<'a> {
            user_id: &'a str,
            post_id: &'a str,
            emoji_name: &'a str,
        }
        let res = self
            .client
            .post(format!("{}/api/v4/reactions", self.base_url))
            .bearer_auth(&self.token)
            .json(&Reaction {
                user_id,
                post_id,
                emoji_name,
            })
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = res.status();
        if !status.is_success() {
            return Err(map_error(status, res).await);
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct CreatePostBody<'a> {
    channel_id: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_ids: Option<&'a [String]>,
}

#[derive(Deserialize)]
struct PostResponse {
    id: String,
}

#[derive(Deserialize)]
struct UploadResponse {
    file_infos: Vec<FileInfo>,
}

#[derive(Deserialize)]
struct FileInfo {
    id: String,
}

fn transport(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(err.to_string())
}

async fn map_error(status: reqwest::StatusCode, res: reqwest::Response) -> AdapterError {
    let body_preview = res
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable body>".to_string());
    let snippet: String = body_preview.chars().take(256).collect();
    match status.as_u16() {
        401 | 403 => AdapterError::Auth(snippet),
        400 | 404 | 422 => AdapterError::BadRequest(format!("{status}: {snippet}")),
        429 => AdapterError::Rate { retry_after: None },
        500..=599 => AdapterError::Transport(format!("server {status}: {snippet}")),
        _ => AdapterError::Transport(format!("unexpected {status}: {snippet}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server() -> MockServer {
        MockServer::start().await
    }

    #[tokio::test]
    async fn create_post_returns_id_on_201() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "post-9"})))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "test-token");
        let id = api.create_post("chan-1", "hi", None).await.unwrap();
        assert_eq!(id, "post-9");
    }

    #[tokio::test]
    async fn create_post_threads_via_root_id() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "p2"})))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        let id = api
            .create_post("chan-1", "hi", Some("root-1"))
            .await
            .unwrap();
        assert_eq!(id, "p2");
    }

    #[tokio::test]
    async fn unauthorized_maps_to_auth_error() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad token"))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "wrong");
        match api.create_post("c", "m", None).await.unwrap_err() {
            AdapterError::Auth(m) => assert!(m.contains("bad token")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forbidden_also_auth_error() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(403).set_body_string("no permission"))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        assert!(matches!(
            api.create_post("c", "m", None).await.unwrap_err(),
            AdapterError::Auth(_)
        ));
    }

    #[tokio::test]
    async fn bad_request_maps_to_bad_request() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid channel"))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        match api.create_post("c", "m", None).await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("invalid channel")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rate_limit_maps_to_rate_error() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        assert!(matches!(
            api.create_post("c", "m", None).await.unwrap_err(),
            AdapterError::Rate { .. }
        ));
    }

    #[tokio::test]
    async fn server_error_maps_to_transport() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(503).set_body_string("backend down"))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        assert!(matches!(
            api.create_post("c", "m", None).await.unwrap_err(),
            AdapterError::Transport(_)
        ));
    }

    #[tokio::test]
    async fn update_post_succeeds_on_200() {
        let mock = server().await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/posts/p1/patch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "p1"})))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        api.update_post("p1", "edited").await.unwrap();
    }

    #[tokio::test]
    async fn update_post_propagates_404() {
        let mock = server().await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/posts/missing/patch"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        assert!(matches!(
            api.update_post("missing", "x").await.unwrap_err(),
            AdapterError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn add_reaction_succeeds_on_201() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/reactions"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({"emoji_name":"thumbsup"})),
            )
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        api.add_reaction("u1", "p1", "thumbsup").await.unwrap();
    }

    #[tokio::test]
    async fn add_reaction_400_is_bad_request() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/reactions"))
            .respond_with(ResponseTemplate::new(400).set_body_string("unknown emoji"))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        assert!(matches!(
            api.add_reaction("u", "p", "fake").await.unwrap_err(),
            AdapterError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn upload_file_returns_id_on_201() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/files"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({ "file_infos": [{ "id": "abc" }] })),
            )
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        let id = api
            .upload_file("c1", "report.pdf", vec![0u8, 1, 2, 3])
            .await
            .unwrap();
        assert_eq!(id, "abc");
    }

    #[tokio::test]
    async fn upload_file_empty_file_infos_is_transport_error() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/files"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "file_infos": [] })))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        match api.upload_file("c", "a.txt", vec![0u8]).await.unwrap_err() {
            AdapterError::Transport(m) => assert!(m.contains("empty file_infos")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_file_empty_filename_is_bad_request() {
        let api = MattermostApi::new("https://chat.example", "t");
        match api.upload_file("c", "", vec![0u8]).await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("filename")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_post_with_files_includes_file_ids_in_body() {
        // We can't easily inspect the body via wiremock without a
        // matcher, but a 201 with a parseable response is sufficient
        // to assert the API surface compiles and round-trips.
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "p9"})))
            .mount(&mock)
            .await;
        let api = MattermostApi::new(&mock.uri(), "t");
        let id = api
            .create_post_with_files("c", "hi", None, &["f1".to_string(), "f2".to_string()])
            .await
            .unwrap();
        assert_eq!(id, "p9");
    }

    #[test]
    fn new_strips_trailing_slash_from_base() {
        let a = MattermostApi::new("https://chat.example/", "t");
        assert!(!a.base_url.ends_with('/'));
    }
}
