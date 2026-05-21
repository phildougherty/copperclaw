//! CRUD for `pending_questions`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{ChannelType, MessageId, QuestionId, SessionId};
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestion {
    pub question_id: QuestionId,
    pub session_id: SessionId,
    pub message_out_id: MessageId,
    pub platform_id: Option<String>,
    pub channel_type: Option<ChannelType>,
    pub thread_id: Option<String>,
    pub title: String,
    pub options: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct InsertPendingQuestion {
    pub session_id: SessionId,
    pub message_out_id: MessageId,
    pub platform_id: Option<String>,
    pub channel_type: Option<ChannelType>,
    pub thread_id: Option<String>,
    pub title: String,
    pub options: Vec<String>,
}

fn row_to_pending_question(row: &Row<'_>) -> rusqlite::Result<PendingQuestion> {
    let question_id_str: String = row.get("question_id")?;
    let question_id = uuid::Uuid::parse_str(&question_id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let session_id_str: String = row.get("session_id")?;
    let session_id = uuid::Uuid::parse_str(&session_id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let message_out_id_str: String = row.get("message_out_id")?;
    let message_out_id = uuid::Uuid::parse_str(&message_out_id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let channel_type: Option<String> = row.get("channel_type")?;
    let options_json: String = row.get("options_json")?;
    let options: Vec<String> = serde_json::from_str(&options_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    Ok(PendingQuestion {
        question_id: QuestionId(question_id),
        session_id: SessionId(session_id),
        message_out_id: MessageId(message_out_id),
        platform_id: row.get("platform_id")?,
        channel_type: channel_type.map(ChannelType::from),
        thread_id: row.get("thread_id")?,
        title: row.get("title")?,
        options,
        created_at,
    })
}

pub fn insert(db: &CentralDb, req: InsertPendingQuestion) -> Result<PendingQuestion, DbError> {
    let id = QuestionId::new();
    let now = Utc::now();
    let options_json = serde_json::to_string(&req.options)?;
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO pending_questions
           (question_id, session_id, message_out_id, platform_id, channel_type,
            thread_id, title, options_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id.as_uuid().to_string(),
            req.session_id.as_uuid().to_string(),
            req.message_out_id.as_uuid().to_string(),
            req.platform_id,
            req.channel_type.as_ref().map(ChannelType::as_str),
            req.thread_id,
            req.title,
            options_json,
            now.to_rfc3339(),
        ],
    )?;
    Ok(PendingQuestion {
        question_id: id,
        session_id: req.session_id,
        message_out_id: req.message_out_id,
        platform_id: req.platform_id,
        channel_type: req.channel_type,
        thread_id: req.thread_id,
        title: req.title,
        options: req.options,
        created_at: now,
    })
}

pub fn get(db: &CentralDb, id: QuestionId) -> Result<PendingQuestion, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT question_id, session_id, message_out_id, platform_id, channel_type,
                thread_id, title, options_json, created_at
         FROM pending_questions WHERE question_id = ?1",
        params![id.as_uuid().to_string()],
        row_to_pending_question,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

pub fn delete(db: &CentralDb, id: QuestionId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM pending_questions WHERE question_id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use crate::tables::sessions::{create as create_session, CreateSession};

    fn db_with_session() -> (CentralDb, SessionId) {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "greeter".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let session = create_session(
            &db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
            },
        )
        .unwrap();
        (db, session.id)
    }

    fn sample(session_id: SessionId) -> InsertPendingQuestion {
        InsertPendingQuestion {
            session_id,
            message_out_id: MessageId::new(),
            platform_id: Some("chat-1".into()),
            channel_type: Some(ChannelType::new("telegram")),
            thread_id: Some("t1".into()),
            title: "Pick one".into(),
            options: vec!["yes".into(), "no".into()],
        }
    }

    #[test]
    fn insert_then_get() {
        let (db, session_id) = db_with_session();
        let q = insert(&db, sample(session_id)).unwrap();
        let fetched = get(&db, q.question_id).unwrap();
        assert_eq!(q, fetched);
        assert_eq!(fetched.title, "Pick one");
        assert_eq!(fetched.options, vec!["yes".to_string(), "no".to_string()]);
        assert_eq!(fetched.session_id, session_id);
        assert_eq!(fetched.channel_type.as_ref().map(ChannelType::as_str), Some("telegram"));
    }

    #[test]
    fn insert_with_empty_options_roundtrips() {
        let (db, session_id) = db_with_session();
        let mut req = sample(session_id);
        req.options = vec![];
        req.platform_id = None;
        req.channel_type = None;
        req.thread_id = None;
        let q = insert(&db, req).unwrap();
        let fetched = get(&db, q.question_id).unwrap();
        assert!(fetched.options.is_empty());
        assert!(fetched.platform_id.is_none());
        assert!(fetched.channel_type.is_none());
        assert!(fetched.thread_id.is_none());
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = get(&db, QuestionId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn insert_without_session_fails() {
        let db = CentralDb::open_in_memory().unwrap();
        let req = InsertPendingQuestion {
            session_id: SessionId::new(),
            message_out_id: MessageId::new(),
            platform_id: None,
            channel_type: None,
            thread_id: None,
            title: "x".into(),
            options: vec![],
        };
        let err = insert(&db, req).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn delete_works() {
        let (db, session_id) = db_with_session();
        let q = insert(&db, sample(session_id)).unwrap();
        delete(&db, q.question_id).unwrap();
        assert!(matches!(get(&db, q.question_id).unwrap_err(), DbError::NotFound));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = delete(&db, QuestionId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }
}
