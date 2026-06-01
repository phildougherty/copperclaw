//! Error types for the `OneCliClient`.

use thiserror::Error;

/// Failures returned by [`crate::OneCliClient`] operations.
///
/// Variants map onto distinct HTTP-status / transport failure modes so that
/// callers can decide whether to retry, surface to the user, or abort.
#[derive(Debug, Error)]
pub enum OneCliError {
    /// A networking / `reqwest` failure occurred before a response could be
    /// inspected (DNS, TLS, connection reset, timeout, etc.).
    #[error("transport: {0}")]
    Transport(String),

    /// Generic non-2xx HTTP response that does not map onto a more specific
    /// variant below.
    #[error("api: HTTP {status}: {message}")]
    Api {
        /// HTTP status code received from `OneCLI`.
        status: u16,
        /// Human-readable message extracted from the response body (or a
        /// fallback when the body could not be parsed).
        message: String,
    },

    /// Response body could not be deserialized into the expected typed shape.
    #[error("decode: {0}")]
    Decode(String),

    /// The bearer token was rejected. Maps onto HTTP `401`.
    #[error("unauthorized")]
    Unauthorized,

    /// A referenced resource does not exist. Maps onto HTTP `404`.
    #[error("not found")]
    NotFound,

    /// The request conflicts with current server state (e.g. a duplicate
    /// slug). Maps onto HTTP `409`.
    #[error("conflict: {message}")]
    Conflict {
        /// Server-supplied detail explaining the conflict.
        message: String,
    },

    /// Client should back off. Maps onto HTTP `429`.
    #[error("rate limited (retry_after={retry_after:?})")]
    RateLimited {
        /// Value of the `Retry-After` header expressed in whole seconds, when
        /// present and parseable.
        retry_after: Option<u64>,
    },

    /// `OneCLI` returned a 5xx response. The contained message is the body
    /// (truncated to a sensible length by the client).
    #[error("server: {0}")]
    Server(String),
}

impl OneCliError {
    /// Returns `true` when the failure is transient and the caller may safely
    /// retry the same request after a back-off.
    ///
    /// Specifically: [`Self::Transport`], [`Self::Server`], and
    /// [`Self::RateLimited`] are retryable. All other variants represent a
    /// definitive client- or contract-level failure.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            OneCliError::Transport(_) | OneCliError::Server(_) | OneCliError::RateLimited { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_transport() {
        let e = OneCliError::Transport("connect refused".into());
        assert_eq!(e.to_string(), "transport: connect refused");
    }

    #[test]
    fn display_api() {
        let e = OneCliError::Api {
            status: 418,
            message: "tea".into(),
        };
        assert_eq!(e.to_string(), "api: HTTP 418: tea");
    }

    #[test]
    fn display_decode() {
        let e = OneCliError::Decode("missing field".into());
        assert_eq!(e.to_string(), "decode: missing field");
    }

    #[test]
    fn display_unauthorized() {
        assert_eq!(OneCliError::Unauthorized.to_string(), "unauthorized");
    }

    #[test]
    fn display_not_found() {
        assert_eq!(OneCliError::NotFound.to_string(), "not found");
    }

    #[test]
    fn display_conflict() {
        let e = OneCliError::Conflict {
            message: "slug exists".into(),
        };
        assert_eq!(e.to_string(), "conflict: slug exists");
    }

    #[test]
    fn display_rate_limited_with_retry_after() {
        let e = OneCliError::RateLimited {
            retry_after: Some(30),
        };
        assert_eq!(e.to_string(), "rate limited (retry_after=Some(30))");
    }

    #[test]
    fn display_rate_limited_without_retry_after() {
        let e = OneCliError::RateLimited { retry_after: None };
        assert_eq!(e.to_string(), "rate limited (retry_after=None)");
    }

    #[test]
    fn display_server() {
        let e = OneCliError::Server("oops".into());
        assert_eq!(e.to_string(), "server: oops");
    }

    #[test]
    fn is_retryable_transport() {
        assert!(OneCliError::Transport("x".into()).is_retryable());
    }

    #[test]
    fn is_retryable_server() {
        assert!(OneCliError::Server("x".into()).is_retryable());
    }

    #[test]
    fn is_retryable_rate_limited() {
        assert!(OneCliError::RateLimited { retry_after: None }.is_retryable());
        assert!(
            OneCliError::RateLimited {
                retry_after: Some(1)
            }
            .is_retryable()
        );
    }

    #[test]
    fn is_retryable_api_is_false() {
        let e = OneCliError::Api {
            status: 400,
            message: "bad".into(),
        };
        assert!(!e.is_retryable());
    }

    #[test]
    fn is_retryable_decode_is_false() {
        assert!(!OneCliError::Decode("x".into()).is_retryable());
    }

    #[test]
    fn is_retryable_unauthorized_is_false() {
        assert!(!OneCliError::Unauthorized.is_retryable());
    }

    #[test]
    fn is_retryable_not_found_is_false() {
        assert!(!OneCliError::NotFound.is_retryable());
    }

    #[test]
    fn is_retryable_conflict_is_false() {
        let e = OneCliError::Conflict {
            message: "x".into(),
        };
        assert!(!e.is_retryable());
    }
}
