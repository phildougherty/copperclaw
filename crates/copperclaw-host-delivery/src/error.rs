//! Errors surfaced by the delivery subsystem.

use copperclaw_channels_core::AdapterError;
use copperclaw_db::DbError;
use copperclaw_types::{ChannelType, SessionId};
use thiserror::Error;

/// Errors produced while processing a session's outbound queue.
///
/// Each variant maps onto a distinct host-side response:
/// - `Db` — a database read/write against the central DB or a per-session DB
///   failed; propagate to the caller.
/// - `Adapter` — a channel adapter call surfaced an [`AdapterError`]. The
///   delivery loop classifies retryable vs. terminal variants on the way back
///   from the loop body.
/// - `NoAdapter` — `messages_out.channel_type` is set but no channel adapter
///   is registered. The row is left in place; the host should warn and either
///   register the adapter or drain the row manually.
/// - `NoRoute` — neither the row nor `session_routing` carries a usable
///   destination. The host should treat this as a configuration bug.
/// - `SystemAction` — a `MessageKind::System` row could not be parsed or no
///   handler exists for its action name.
#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error("db: {0}")]
    Db(#[from] DbError),

    #[error("adapter: {0}")]
    Adapter(#[from] AdapterError),

    #[error("no adapter registered for channel `{0}`")]
    NoAdapter(ChannelType),

    #[error("no routing target for session {0}")]
    NoRoute(SessionId),

    #[error("system action error: {0}")]
    SystemAction(String),
}

impl DeliveryError {
    /// When the underlying error is an [`AdapterError::Rate`] carrying a
    /// `retry_after` hint (seconds), return it. Otherwise `None`. Used by
    /// the delivery loop to honour platform-supplied backoff windows in
    /// place of the fixed exponential schedule.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::Adapter(AdapterError::Rate { retry_after }) => *retry_after,
            _ => None,
        }
    }

    /// True when this error is worth retrying (transport blip, rate limit).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Adapter(err) => matches!(
                err,
                AdapterError::Rate { .. } | AdapterError::Transport(_) | AdapterError::Io(_)
            ),
            // Database failures may be transient (e.g. lock contention), but
            // delivery doesn't retry them in-line — surface to the caller.
            Self::Db(_) | Self::NoAdapter(_) | Self::NoRoute(_) | Self::SystemAction(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_variant_displays_inner() {
        let err: DeliveryError = DbError::NotFound.into();
        assert!(format!("{err}").contains("not found"));
        assert!(matches!(err, DeliveryError::Db(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn adapter_variant_displays_inner() {
        let err: DeliveryError = AdapterError::Transport("oops".into()).into();
        assert!(format!("{err}").contains("oops"));
        assert!(matches!(err, DeliveryError::Adapter(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn adapter_rate_is_retryable() {
        let err: DeliveryError = AdapterError::Rate { retry_after: Some(2) }.into();
        assert!(err.is_retryable());
    }

    #[test]
    fn adapter_io_is_retryable() {
        let err: DeliveryError =
            AdapterError::Io(std::io::Error::new(std::io::ErrorKind::Other, "x")).into();
        assert!(err.is_retryable());
    }

    #[test]
    fn adapter_auth_is_not_retryable() {
        let err: DeliveryError = AdapterError::Auth("bad token".into()).into();
        assert!(!err.is_retryable());
    }

    #[test]
    fn adapter_bad_request_is_not_retryable() {
        let err: DeliveryError = AdapterError::BadRequest("nope".into()).into();
        assert!(!err.is_retryable());
    }

    #[test]
    fn adapter_unsupported_is_not_retryable() {
        let err: DeliveryError = AdapterError::Unsupported("dms".into()).into();
        assert!(!err.is_retryable());
    }

    #[test]
    fn adapter_not_implemented_is_not_retryable() {
        let err: DeliveryError = AdapterError::NotImplemented.into();
        assert!(!err.is_retryable());
    }

    #[test]
    fn no_adapter_variant_displays() {
        let err = DeliveryError::NoAdapter(ChannelType::new("ghost"));
        assert!(format!("{err}").contains("ghost"));
        assert!(!err.is_retryable());
    }

    #[test]
    fn no_route_variant_displays() {
        let err = DeliveryError::NoRoute(SessionId::nil());
        assert!(format!("{err}").contains("no routing target"));
        assert!(!err.is_retryable());
    }

    #[test]
    fn system_action_variant_displays() {
        let err = DeliveryError::SystemAction("unknown action `foo`".into());
        assert!(format!("{err}").contains("unknown action"));
        assert!(!err.is_retryable());
    }
}
