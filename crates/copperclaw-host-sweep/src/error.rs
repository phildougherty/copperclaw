//! Errors produced by the sweep service.

use thiserror::Error;

/// Failure modes for [`crate::SweepService`] passes.
#[derive(Debug, Error)]
pub enum SweepError {
    /// Failure originating from the central or a per-session database.
    #[error("db error: {0}")]
    Db(#[from] copperclaw_db::DbError),
    /// Direct rusqlite error path, used when we operate on a raw connection.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Failure parsing a `messages_in.recurrence` schedule string.
    #[error("schedule parse error: {0}")]
    ScheduleParse(String),
    /// Filesystem failure (heartbeat stat, attachment path, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_error_propagates() {
        let inner = copperclaw_db::DbError::NotFound;
        let wrapped: SweepError = inner.into();
        assert!(matches!(wrapped, SweepError::Db(_)));
        assert!(wrapped.to_string().contains("db error"));
    }

    #[test]
    fn sqlite_error_propagates() {
        let inner = rusqlite::Error::QueryReturnedNoRows;
        let wrapped: SweepError = inner.into();
        assert!(matches!(wrapped, SweepError::Sqlite(_)));
    }

    #[test]
    fn io_error_propagates() {
        let inner = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        let wrapped: SweepError = inner.into();
        assert!(matches!(wrapped, SweepError::Io(_)));
    }

    #[test]
    fn schedule_parse_carries_message() {
        let err = SweepError::ScheduleParse("bad cron".into());
        assert_eq!(err.to_string(), "schedule parse error: bad cron");
    }
}
