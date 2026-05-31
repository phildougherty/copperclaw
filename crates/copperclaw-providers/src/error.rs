//! Error type for agent providers.
//!
//! Every provider implementation maps its transport / API failures onto
//! these variants. The runner uses [`ProviderError::is_retryable`] to decide
//! whether to back off and retry the turn.

use thiserror::Error;

/// Errors that can be produced by an [`crate::AgentProvider`] or
/// [`crate::AgentQuery`] implementation.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Lower-level transport failure (DNS, TCP, TLS, broken stream, etc.).
    #[error("transport error: {0}")]
    Transport(String),

    /// The continuation token (or session id) passed in
    /// [`crate::QueryInput::previous_continuation`] is no longer valid and
    /// the caller must start a fresh session.
    #[error("session invalidated")]
    SessionInvalid,

    /// The upstream API returned a non-success status with a message body.
    #[error("api error {status}: {message}")]
    Api {
        /// HTTP status code (or provider-native equivalent).
        status: u16,
        /// Human-readable error message from the upstream payload.
        message: String,
    },

    /// Failure decoding the upstream response (malformed SSE, bad JSON, …).
    #[error("decode error: {0}")]
    Decode(String),

    /// The caller aborted the in-flight query via
    /// [`crate::AgentQuery::abort`] or by dropping the future.
    #[error("cancelled")]
    Cancelled,

    /// The upstream signalled overload (typically HTTP 429 / 529 from
    /// Anthropic, or an explicit `overloaded_error` event).
    #[error("overloaded")]
    Overloaded,

    /// The request was rejected as malformed (e.g. HTTP 400). The string
    /// carries the upstream message.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The runner's per-call deadline elapsed (after all retries) before
    /// the provider produced a response. Terminal — the runner has
    /// already given up by the time this variant is constructed.
    ///
    /// Carries the deadline in milliseconds and the attempt count that
    /// was reached so callers / log scrapers can see how many shots
    /// were taken before giving up.
    #[error("provider deadline exceeded after {attempts} attempt(s) ({deadline_ms} ms each)")]
    DeadlineExceeded {
        /// Per-call deadline that tripped, in milliseconds.
        deadline_ms: u64,
        /// Number of attempts that were made before giving up.
        attempts: u32,
    },
}

impl ProviderError {
    /// True if the caller should retry the operation after a backoff.
    ///
    /// The mapping is conservative:
    /// * [`Self::Transport`] and [`Self::Overloaded`] are retryable.
    /// * [`Self::Api`] is retryable only for 5xx status codes.
    /// * Everything else is fatal for the turn.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Overloaded => true,
            Self::Api { status, .. } => *status >= 500,
            Self::SessionInvalid
            | Self::Decode(_)
            | Self::Cancelled
            | Self::BadRequest(_)
            | Self::DeadlineExceeded { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_transport() {
        let e = ProviderError::Transport("connection reset".into());
        assert_eq!(e.to_string(), "transport error: connection reset");
    }

    #[test]
    fn display_session_invalid() {
        assert_eq!(ProviderError::SessionInvalid.to_string(), "session invalidated");
    }

    #[test]
    fn display_api() {
        let e = ProviderError::Api { status: 503, message: "service down".into() };
        assert_eq!(e.to_string(), "api error 503: service down");
    }

    #[test]
    fn display_decode() {
        let e = ProviderError::Decode("bad json".into());
        assert_eq!(e.to_string(), "decode error: bad json");
    }

    #[test]
    fn display_cancelled() {
        assert_eq!(ProviderError::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn display_overloaded() {
        assert_eq!(ProviderError::Overloaded.to_string(), "overloaded");
    }

    #[test]
    fn display_bad_request() {
        let e = ProviderError::BadRequest("missing model".into());
        assert_eq!(e.to_string(), "bad request: missing model");
    }

    #[test]
    fn retryable_transport() {
        assert!(ProviderError::Transport("x".into()).is_retryable());
    }

    #[test]
    fn retryable_overloaded() {
        assert!(ProviderError::Overloaded.is_retryable());
    }

    #[test]
    fn retryable_api_5xx() {
        assert!(ProviderError::Api { status: 500, message: "x".into() }.is_retryable());
        assert!(ProviderError::Api { status: 502, message: "x".into() }.is_retryable());
        assert!(ProviderError::Api { status: 599, message: "x".into() }.is_retryable());
    }

    #[test]
    fn not_retryable_api_4xx() {
        assert!(!ProviderError::Api { status: 400, message: "x".into() }.is_retryable());
        assert!(!ProviderError::Api { status: 401, message: "x".into() }.is_retryable());
        assert!(!ProviderError::Api { status: 404, message: "x".into() }.is_retryable());
        assert!(!ProviderError::Api { status: 499, message: "x".into() }.is_retryable());
    }

    #[test]
    fn not_retryable_session_invalid() {
        assert!(!ProviderError::SessionInvalid.is_retryable());
    }

    #[test]
    fn not_retryable_decode() {
        assert!(!ProviderError::Decode("x".into()).is_retryable());
    }

    #[test]
    fn not_retryable_cancelled() {
        assert!(!ProviderError::Cancelled.is_retryable());
    }

    #[test]
    fn not_retryable_bad_request() {
        assert!(!ProviderError::BadRequest("x".into()).is_retryable());
    }

    #[test]
    fn display_deadline_exceeded() {
        let e = ProviderError::DeadlineExceeded { deadline_ms: 1000, attempts: 3 };
        assert_eq!(
            e.to_string(),
            "provider deadline exceeded after 3 attempt(s) (1000 ms each)"
        );
    }

    #[test]
    fn not_retryable_deadline_exceeded() {
        let e = ProviderError::DeadlineExceeded { deadline_ms: 1000, attempts: 3 };
        assert!(!e.is_retryable());
    }
}
