//! Errors returned by channel adapters and factories.

use thiserror::Error;

/// Errors that channel adapters and factories may surface.
///
/// Every variant maps to a distinct host-side response policy:
/// - `Io` — propagated transport-layer I/O failure (sockets, pipes).
/// - `Transport(String)` — non-IO transport error (HTTP non-success, websocket close).
/// - `Auth(String)` — credentials missing, expired, or rejected by the platform.
/// - `Rate { retry_after }` — rate-limited; honor `retry_after` (seconds) if present.
/// - `BadRequest(String)` — adapter rejected the call (malformed input, unsupported field).
/// - `NotImplemented` — the adapter chose not to implement this trait method.
/// - `Unsupported(String)` — the platform does not support the requested operation.
#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport: {0}")]
    Transport(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("rate limited{}", retry_after.map_or(String::new(), |s| format!(" (retry after {s}s)")))]
    Rate { retry_after: Option<u64> },
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("not implemented")]
    NotImplemented,
    #[error("unsupported: {0}")]
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_variant_displays_inner_error() {
        let io = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        let err: AdapterError = io.into();
        assert!(format!("{err}").contains("boom"));
        assert!(matches!(err, AdapterError::Io(_)));
    }

    #[test]
    fn transport_variant_displays() {
        let err = AdapterError::Transport("502 bad gateway".into());
        assert_eq!(format!("{err}"), "transport: 502 bad gateway");
    }

    #[test]
    fn auth_variant_displays() {
        let err = AdapterError::Auth("token expired".into());
        assert_eq!(format!("{err}"), "auth: token expired");
    }

    #[test]
    fn rate_variant_without_retry_after() {
        let err = AdapterError::Rate { retry_after: None };
        assert_eq!(format!("{err}"), "rate limited");
    }

    #[test]
    fn rate_variant_with_retry_after() {
        let err = AdapterError::Rate {
            retry_after: Some(30),
        };
        assert_eq!(format!("{err}"), "rate limited (retry after 30s)");
    }

    #[test]
    fn bad_request_variant_displays() {
        let err = AdapterError::BadRequest("missing field".into());
        assert_eq!(format!("{err}"), "bad request: missing field");
    }

    #[test]
    fn not_implemented_variant_displays() {
        let err = AdapterError::NotImplemented;
        assert_eq!(format!("{err}"), "not implemented");
    }

    #[test]
    fn unsupported_variant_displays() {
        let err = AdapterError::Unsupported("dms".into());
        assert_eq!(format!("{err}"), "unsupported: dms");
    }

    #[test]
    fn debug_format_is_available() {
        let err = AdapterError::NotImplemented;
        let dbg = format!("{err:?}");
        assert!(dbg.contains("NotImplemented"));
    }
}
