//! Router error type. Wraps DB / IO / hook failures.

use copperclaw_db::DbError;
use thiserror::Error;

/// Errors the router can surface to its caller.
///
/// Every variant is constructible directly so test harnesses can synthesize
/// failures; the `From` impls below let the routing pipeline use `?` against
/// the dependency error types it touches.
#[derive(Debug, Error)]
pub enum RouterError {
    /// A central- or session-DB query failed.
    #[error("db error: {0}")]
    Db(#[from] DbError),

    /// One of the registered hook closures panicked. The hook chain catches
    /// panics so the router can continue serving other events.
    #[error("hook panicked: {0}")]
    HookPanicked(String),

    /// A session row exists but the on-disk session directory could not be
    /// created or migrated.
    #[error("session create failed: {0}")]
    SessionCreate(String),

    /// The router was asked to deliver an event for a `(channel_type,
    /// platform_id)` that has no `messaging_groups` row. Surfaced as an
    /// error only when the caller explicitly asked for one; the default
    /// routing path turns this case into [`crate::DropReason::NoMessagingGroup`].
    #[error("no messaging group for event")]
    NoMessagingGroup,

    /// A wiring row referenced an agent group that doesn't exist, or carried
    /// an invalid combination of fields the router cannot satisfy.
    #[error("invalid wiring: {0}")]
    InvalidWiring(String),

    /// An I/O failure not covered by [`RouterError::SessionCreate`] — e.g. a
    /// session pool open against an existing directory failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl RouterError {
    /// Construct a `SessionCreate` from any error-displaying value.
    pub fn session_create(msg: impl Into<String>) -> Self {
        Self::SessionCreate(msg.into())
    }

    /// Construct an `InvalidWiring` from a message.
    pub fn invalid_wiring(msg: impl Into<String>) -> Self {
        Self::InvalidWiring(msg.into())
    }

    /// Construct a `HookPanicked` from a message.
    pub fn hook_panicked(msg: impl Into<String>) -> Self {
        Self::HookPanicked(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_error_converts() {
        let err: RouterError = DbError::NotFound.into();
        assert!(matches!(err, RouterError::Db(DbError::NotFound)));
        assert!(err.to_string().contains("db error"));
    }

    #[test]
    fn io_error_converts() {
        let err: RouterError =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope").into();
        assert!(matches!(err, RouterError::Io(_)));
        assert!(err.to_string().contains("io error"));
    }

    #[test]
    fn invalid_wiring_constructor() {
        let err = RouterError::invalid_wiring("bad");
        assert!(matches!(err, RouterError::InvalidWiring(_)));
        assert_eq!(err.to_string(), "invalid wiring: bad");
    }

    #[test]
    fn session_create_constructor() {
        let err = RouterError::session_create("could not mkdir");
        assert!(matches!(err, RouterError::SessionCreate(_)));
        assert_eq!(err.to_string(), "session create failed: could not mkdir");
    }

    #[test]
    fn hook_panicked_constructor() {
        let err = RouterError::hook_panicked("panicked");
        assert!(matches!(err, RouterError::HookPanicked(_)));
        assert_eq!(err.to_string(), "hook panicked: panicked");
    }

    #[test]
    fn no_messaging_group_renders() {
        let err = RouterError::NoMessagingGroup;
        assert_eq!(err.to_string(), "no messaging group for event");
    }
}
