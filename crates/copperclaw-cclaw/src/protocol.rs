//! Wire protocol for the `cclaw` Unix-domain-socket transport.
//!
//! See `PLAN.md` § 5.4. Newline-delimited JSON, one request and one response
//! per connection. Both the host (M5 socket server) and the `cclaw` binary
//! depend on this module so that the wire format has exactly one source of
//! truth.
//!
//! # Framing
//!
//! Every frame is a single JSON object terminated by an ASCII line-feed
//! (`b"\n"`). Embedded newlines inside the payload are not permitted — the
//! payload is produced via [`serde_json::to_vec`] which never emits raw
//! newlines.

use copperclaw_types::{AgentGroupId, MessagingGroupId, SessionId};
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// A `cclaw` request. `kind: "call"` is the only variant; the enum is kept
/// open so the host can introduce additional shapes (e.g. streaming) later
/// without a breaking format change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Request {
    /// A single command invocation.
    Call {
        /// Caller-generated correlation id echoed back in the response.
        id: String,
        /// Dotted command name (e.g. `"groups.list"`).
        command: String,
        /// Command-specific argument payload.
        args: serde_json::Value,
        /// Who is making the call.
        caller: Caller,
    },
}

/// Identity of the caller behind a request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Caller {
    /// Admin context — the call originated from the local socket and the
    /// caller has full privileges.
    Host,
    /// An agent inside a container. Agent calls actually flow through
    /// session DBs in production, but exposing the shape on the wire lets
    /// us test handlers end-to-end without spinning up a container.
    Agent {
        session_id: SessionId,
        agent_group_id: AgentGroupId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        messaging_group_id: Option<MessagingGroupId>,
    },
}

/// A `cclaw` response. Matches `Request` by `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Response {
    /// The call succeeded; `data` holds the command-specific payload.
    Ok {
        id: String,
        data: serde_json::Value,
    },
    /// The call failed.
    Err { id: String, error: ErrorPayload },
}

/// Structured error returned alongside a [`Response::Err`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorPayload {
    /// Stable machine-readable code (e.g. `"not-found"`, `"unauthorized"`).
    pub code: String,
    /// Human-readable message. Free-form.
    pub message: String,
    /// Whether the caller may retry without further intervention.
    pub retryable: bool,
    /// Optional command-specific structured data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Errors raised by the framing helpers.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// Underlying I/O failure (socket closed mid-frame, etc.).
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// The frame parsed as bytes but not as JSON.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The peer half-closed without sending another frame.
    #[error("peer closed the connection")]
    Closed,
}

/// Read one newline-delimited JSON `Request` from `stream`.
///
/// Returns [`ProtoError::Closed`] if the peer closes without writing
/// further bytes, [`ProtoError::Json`] if the bytes are not valid JSON,
/// and [`ProtoError::Io`] for anything else.
pub async fn read_request<R>(stream: &mut R) -> Result<Request, ProtoError>
where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    read_frame(stream).await
}

/// Read one newline-delimited JSON `Response` from `stream`.
pub async fn read_response<R>(stream: &mut R) -> Result<Response, ProtoError>
where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    read_frame(stream).await
}

/// Generic newline-delimited JSON frame reader.
///
/// Reads bytes up to and including the first `\n`, then deserializes the
/// preceding bytes as `T`. The trailing newline is stripped before parsing.
pub async fn read_frame<R, T>(stream: &mut R) -> Result<T, ProtoError>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    T: serde::de::DeserializeOwned,
{
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::with_capacity(256);
    let n = reader.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Err(ProtoError::Closed);
    }
    // Strip trailing newline if present.
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.is_empty() {
        return Err(ProtoError::Closed);
    }
    let value = serde_json::from_slice(&buf)?;
    Ok(value)
}

/// Write a `Request` frame to `stream`.
pub async fn write_request<W>(stream: &mut W, req: &Request) -> Result<(), ProtoError>
where
    W: AsyncWrite + Unpin + Send,
{
    write_frame(stream, req).await
}

/// Write a `Response` frame to `stream`.
pub async fn write_response<W>(stream: &mut W, resp: &Response) -> Result<(), ProtoError>
where
    W: AsyncWrite + Unpin + Send,
{
    write_frame(stream, resp).await
}

/// Generic newline-delimited JSON frame writer.
pub async fn write_frame<W, T>(stream: &mut W, value: &T) -> Result<(), ProtoError>
where
    W: AsyncWrite + Unpin + Send,
    T: serde::Serialize,
{
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    stream.write_all(&buf).await?;
    stream.flush().await?;
    Ok(())
}

impl Response {
    /// Construct an `Ok` response.
    pub fn ok(id: impl Into<String>, data: serde_json::Value) -> Self {
        Self::Ok {
            id: id.into(),
            data,
        }
    }

    /// Construct an `Err` response.
    pub fn err(id: impl Into<String>, error: ErrorPayload) -> Self {
        Self::Err {
            id: id.into(),
            error,
        }
    }

    /// The correlation id common to both variants.
    pub fn id(&self) -> &str {
        match self {
            Self::Ok { id, .. } | Self::Err { id, .. } => id,
        }
    }
}

impl ErrorPayload {
    /// Build a new error payload. `retryable=false` and `data=None`.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            data: None,
        }
    }

    /// Mark this error as retryable.
    #[must_use]
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    /// Attach a structured data blob.
    #[must_use]
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::{AgentGroupId, SessionId};
    use serde_json::json;
    use tokio::io::duplex;

    #[test]
    fn request_call_roundtrip() {
        let req = Request::Call {
            id: "abc".into(),
            command: "groups.list".into(),
            args: json!({}),
            caller: Caller::Host,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
        assert!(s.contains("\"kind\":\"call\""));
    }

    #[test]
    fn caller_host_serializes_kebab() {
        let c = Caller::Host;
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"kind\":\"host\""));
    }

    #[test]
    fn caller_agent_with_optional_mg() {
        let c = Caller::Agent {
            session_id: SessionId::nil(),
            agent_group_id: AgentGroupId::nil(),
            messaging_group_id: None,
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains("messaging_group_id"));
        let back: Caller = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn response_ok_and_err_roundtrip() {
        let ok = Response::ok("1", json!({"hello": "world"}));
        let s = serde_json::to_string(&ok).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert_eq!(ok, back);
        assert_eq!(ok.id(), "1");

        let err = Response::err("2", ErrorPayload::new("not-found", "missing"));
        let s = serde_json::to_string(&err).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert_eq!(err, back);
        assert_eq!(err.id(), "2");
    }

    #[test]
    fn error_payload_builders() {
        let e = ErrorPayload::new("c", "m")
            .retryable()
            .with_data(json!({"k": 1}));
        assert!(e.retryable);
        assert_eq!(e.code, "c");
        assert_eq!(e.message, "m");
        assert_eq!(e.data.unwrap(), json!({"k": 1}));
    }

    #[tokio::test]
    async fn read_write_request_roundtrip() {
        let (mut a, mut b) = duplex(1024);
        let req = Request::Call {
            id: "x".into(),
            command: "groups.get".into(),
            args: json!({"id": "ag_123"}),
            caller: Caller::Host,
        };

        let req_clone = req.clone();
        let writer = tokio::spawn(async move {
            write_request(&mut a, &req_clone).await.unwrap();
        });
        let got = read_request(&mut b).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got, req);
    }

    #[tokio::test]
    async fn read_write_response_roundtrip() {
        let (mut a, mut b) = duplex(1024);
        let resp = Response::ok("y", json!([1, 2, 3]));
        let resp_clone = resp.clone();
        let writer = tokio::spawn(async move {
            write_response(&mut a, &resp_clone).await.unwrap();
        });
        let got = read_response(&mut b).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got, resp);
    }

    #[tokio::test]
    async fn read_bad_json_returns_json_error() {
        let (mut a, mut b) = duplex(64);
        tokio::spawn(async move {
            tokio::io::AsyncWriteExt::write_all(&mut a, b"not json\n")
                .await
                .unwrap();
        });
        let err = read_request(&mut b).await.unwrap_err();
        assert!(matches!(err, ProtoError::Json(_)));
    }

    #[tokio::test]
    async fn read_closed_stream_returns_closed() {
        let (a, mut b) = duplex(64);
        drop(a);
        let err = read_request(&mut b).await.unwrap_err();
        assert!(matches!(err, ProtoError::Closed));
    }

    #[tokio::test]
    async fn read_empty_line_returns_closed() {
        let (mut a, mut b) = duplex(64);
        tokio::spawn(async move {
            tokio::io::AsyncWriteExt::write_all(&mut a, b"\n")
                .await
                .unwrap();
        });
        let err = read_request(&mut b).await.unwrap_err();
        assert!(matches!(err, ProtoError::Closed));
    }

    #[test]
    fn proto_error_display_covers_variants() {
        let io = ProtoError::Io(io::Error::other("boom"));
        let json = ProtoError::Json(serde_json::from_str::<Request>("nope").unwrap_err());
        let closed = ProtoError::Closed;
        assert!(io.to_string().contains("io"));
        assert!(json.to_string().contains("json"));
        assert!(closed.to_string().contains("closed"));
    }
}
