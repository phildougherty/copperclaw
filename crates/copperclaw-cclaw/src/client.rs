//! Unix-socket client for the `cclaw` wire protocol.
//!
//! The host (M5) exposes a Unix-domain-socket server at `data/cclaw.sock`
//! (mode `0o600`) speaking the framing defined in [`crate::protocol`]. The
//! [`CclawClient`] in this module is the canonical client implementation —
//! the `cclaw` binary uses it, and so does anything else that wants to drive
//! the host programmatically (tests, future tooling).
//!
//! A `call` opens a fresh connection, writes one [`Request::Call`] frame,
//! reads one [`Response`] frame, then drops the connection. Half-close per
//! request is intentional: it keeps the server side trivially stateless
//! and avoids subtle issues around connection re-use across reconnects.

use crate::protocol::{
    Caller, ErrorPayload, ProtoError, Request, Response, read_response, write_request,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use uuid::Uuid;

/// Maximum total time spent waiting on a single round-trip before
/// surfacing [`ClientError::Timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Client wrapper around a Unix-domain socket path.
#[derive(Debug, Clone)]
pub struct CclawClient {
    socket_path: PathBuf,
    timeout: Duration,
}

impl CclawClient {
    /// Build a client targeting `socket_path` with the [`DEFAULT_TIMEOUT`].
    ///
    /// No connection is opened until [`Self::call`] is invoked.
    pub fn connect(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Override the per-call timeout. The default is [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Path to the socket this client will dial.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Configured per-call timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Perform a single request/response round-trip.
    ///
    /// On `Response::Ok` the inner `data` payload is returned. On
    /// `Response::Err` the [`ErrorPayload`] is surfaced as
    /// [`ClientError::Remote`]. Transport-level problems become
    /// [`ClientError::Io`] / [`ClientError::Proto`]; exceeding the
    /// configured deadline yields [`ClientError::Timeout`].
    pub async fn call(
        &self,
        command: &str,
        args: serde_json::Value,
        caller: Caller,
    ) -> Result<serde_json::Value, ClientError> {
        let id = Uuid::new_v4().to_string();
        self.call_with_id(&id, command, args, caller).await
    }

    /// Lower-level variant of [`Self::call`] that uses a caller-supplied id.
    pub async fn call_with_id(
        &self,
        id: &str,
        command: &str,
        args: serde_json::Value,
        caller: Caller,
    ) -> Result<serde_json::Value, ClientError> {
        let req = Request::Call {
            id: id.to_string(),
            command: command.to_string(),
            args,
            caller,
        };
        let fut = self.round_trip(req);
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(outcome) => outcome,
            Err(_) => Err(ClientError::Timeout),
        }
    }

    async fn round_trip(&self, req: Request) -> Result<serde_json::Value, ClientError> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        write_request(&mut stream, &req).await?;
        // Half-close the write side so the server sees EOF and may stop reading.
        // The server's response is still readable.
        let (mut read_half, mut write_half) = stream.into_split();
        tokio::io::AsyncWriteExt::shutdown(&mut write_half)
            .await
            .map_err(ProtoError::Io)?;
        let resp = read_response(&mut read_half).await?;
        // The caller-supplied id must match.
        let Request::Call { id, .. } = req;
        if resp.id() != id {
            return Err(ClientError::Proto(ProtoError::Json(
                serde::de::Error::custom(format!(
                    "id mismatch: sent {id:?}, got {got:?}",
                    got = resp.id()
                )),
            )));
        }
        match resp {
            Response::Ok { data, .. } => Ok(data),
            Response::Err { error, .. } => Err(ClientError::Remote(error)),
        }
    }
}

/// Errors returned by [`CclawClient::call`].
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Underlying socket I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The protocol layer reported an error (frame parse, etc.).
    #[error("protocol: {0}")]
    Proto(ProtoError),
    /// The server returned a structured error response.
    #[error("remote: {} - {}", .0.code, .0.message)]
    Remote(ErrorPayload),
    /// The round-trip exceeded the configured timeout.
    #[error("timed out")]
    Timeout,
}

impl From<ProtoError> for ClientError {
    fn from(value: ProtoError) -> Self {
        match value {
            ProtoError::Io(e) => Self::Io(e),
            other => Self::Proto(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{read_request, write_response};
    use serde_json::json;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::net::UnixListener;

    fn temp_socket() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cclaw.sock");
        (dir, path)
    }

    /// Bind a Unix-socket listener synchronously, then spawn a task that
    /// accepts exactly one connection, reads a request, writes the
    /// response produced by `response_for`, and returns the request seen.
    fn spawn_server(
        path: &Path,
        response_for: impl FnOnce(Request) -> Response + Send + 'static,
    ) -> tokio::task::JoinHandle<Request> {
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let req = read_request(&mut stream).await.unwrap();
            let resp = response_for(req.clone());
            write_response(&mut stream, &resp).await.unwrap();
            req
        })
    }

    #[tokio::test]
    async fn call_round_trip_ok() {
        let (_dir, path) = temp_socket();
        let server = spawn_server(&path, |req| {
            let Request::Call { id, .. } = req;
            Response::ok(id, json!({"ok": true}))
        });

        let client = CclawClient::connect(path);
        let data = client
            .call("groups.list", json!({}), Caller::Host)
            .await
            .unwrap();
        assert_eq!(data, json!({"ok": true}));

        let req = server.await.unwrap();
        let Request::Call { command, .. } = req;
        assert_eq!(command, "groups.list");
    }

    #[tokio::test]
    async fn call_remote_error_surfaces() {
        let (_dir, path) = temp_socket();
        let _server = spawn_server(&path, |req| {
            let Request::Call { id, .. } = req;
            Response::err(id, ErrorPayload::new("not-found", "missing"))
        });

        let client = CclawClient::connect(path);
        let err = client
            .call("groups.get", json!({"id": "x"}), Caller::Host)
            .await
            .unwrap_err();
        match err {
            ClientError::Remote(p) => {
                assert_eq!(p.code, "not-found");
                assert_eq!(p.message, "missing");
            }
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_timeout_when_server_does_not_respond() {
        let (_dir, path) = temp_socket();
        let listener = UnixListener::bind(&path).unwrap();
        let _server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            // Hold the connection open without responding.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let client = CclawClient::connect(path).with_timeout(Duration::from_millis(50));
        let err = client
            .call("groups.list", json!({}), Caller::Host)
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Timeout));
    }

    #[tokio::test]
    async fn call_io_error_when_socket_missing() {
        let (dir, path) = temp_socket();
        // Don't bind a listener.
        drop(dir);
        let client = CclawClient::connect(path);
        let err = client
            .call("anything", json!({}), Caller::Host)
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Io(_)));
    }

    #[tokio::test]
    async fn call_id_mismatch_surfaces_proto_error() {
        let (_dir, path) = temp_socket();
        let _server = spawn_server(&path, |_req| Response::ok("wrong-id", json!({})));
        let client = CclawClient::connect(path);
        let err = client
            .call("groups.list", json!({}), Caller::Host)
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Proto(_)));
    }

    #[test]
    fn accessors() {
        let p = PathBuf::from("/tmp/cclaw.sock");
        let c = CclawClient::connect(p.clone()).with_timeout(Duration::from_secs(3));
        assert_eq!(c.socket_path(), p.as_path());
        assert_eq!(c.timeout(), Duration::from_secs(3));
    }

    #[test]
    fn client_error_display() {
        let io = ClientError::Io(std::io::Error::other("boom"));
        assert!(io.to_string().contains("io"));
        let remote = ClientError::Remote(ErrorPayload::new("code-x", "msg-x"));
        assert!(remote.to_string().contains("code-x"));
        let to = ClientError::Timeout;
        assert!(to.to_string().contains("timed out"));
        let proto = ClientError::Proto(ProtoError::Closed);
        assert!(proto.to_string().contains("protocol"));
    }

    #[test]
    fn proto_error_io_maps_to_client_io() {
        let p = ProtoError::Io(std::io::Error::other("x"));
        let c: ClientError = p.into();
        assert!(matches!(c, ClientError::Io(_)));
    }

    #[test]
    fn proto_error_other_maps_to_client_proto() {
        let p = ProtoError::Closed;
        let c: ClientError = p.into();
        assert!(matches!(c, ClientError::Proto(_)));
    }
}
