use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn nil() -> Self {
                Self(Uuid::nil())
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", $prefix, self.0)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

id_type!(AgentGroupId, "ag");
id_type!(MessagingGroupId, "mg");
id_type!(SessionId, "sess");
id_type!(UserId, "u");
id_type!(MessageId, "msg");
id_type!(QuestionId, "q");
id_type!(ApprovalId, "appr");
id_type!(TaskId, "task");
id_type!(WiringId, "wire");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_v7_and_sortable() {
        let a = AgentGroupId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = AgentGroupId::new();
        assert!(a.0 < b.0, "v7 uuids should be time-sortable: a={a} b={b}");
    }

    #[test]
    fn display_prefix() {
        let a = AgentGroupId::nil();
        assert!(a.to_string().starts_with("ag_"));
    }

    #[test]
    fn serde_roundtrip() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}
