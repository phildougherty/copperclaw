use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("pool error: {0}")]
    Pool(#[from] r2d2::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found")]
    NotFound,
    #[error("invariant violated: {0}")]
    Invariant(String),
    #[error("migration `{name}` failed: {source}")]
    Migration {
        name: String,
        #[source]
        source: rusqlite::Error,
    },
}

impl DbError {
    pub fn invariant(msg: impl Into<String>) -> Self {
        Self::Invariant(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invariant_constructor_works() {
        let err = DbError::invariant("seq parity broken");
        assert!(matches!(err, DbError::Invariant(_)));
        assert_eq!(err.to_string(), "invariant violated: seq parity broken");
    }

    #[test]
    fn sqlite_error_propagates() {
        let err: DbError = rusqlite::Error::QueryReturnedNoRows.into();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
