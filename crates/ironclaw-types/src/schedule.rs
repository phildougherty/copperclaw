use crate::id::{SessionId, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Active,
    Paused,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: TaskId,
    pub session_id: SessionId,
    pub name: String,
    pub prompt: String,
    pub when: Option<DateTime<Utc>>,
    pub recurrence: Option<String>,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_serde() {
        for s in [
            TaskStatus::Pending,
            TaskStatus::Active,
            TaskStatus::Paused,
            TaskStatus::Completed,
            TaskStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }
}
