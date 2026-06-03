//! Handlers for every `cclaw` command in [`copperclaw_cclaw::ALL_COMMANDS`].
//!
//! Each handler maps a JSON arg payload onto an `copperclaw-db` table function
//! and returns a JSON response. The socket server in [`crate::socket`] wires
//! these into a [`crate::socket::DispatchTable`] keyed by the dotted command
//! name.
//!
//! ## Caller scope policy
//!
//! Every leaf command is classified as "host-only" or "anyone-can-call":
//!
//! - **Host-only** (mutations + sensitive reads): every command whose dotted
//!   name appears in [`HOST_ONLY_COMMANDS`]. Calling these as
//!   `Caller::Agent` returns `ErrorPayload { code: "permission_denied" }`.
//! - **Anyone**: every other command. Agents can list/read state.
//!
//! In practice almost every mutation requires `Caller::Host`. The complete
//! list lives in [`HOST_ONLY_COMMANDS`] for grep-ability.

use copperclaw_cclaw::{Caller, ErrorPayload};
use copperclaw_types::AgentGroupId;
use serde_json::Value;

pub mod approvals;
pub mod audit;
pub mod budgets;
pub mod db;
pub mod destinations;
pub mod dropped_messages;
pub mod groups;
pub mod mcp;
pub mod members;
pub mod messaging_groups;
pub mod roles;
pub mod schema;
pub mod sessions;
pub mod usage;
pub mod user_dms;
pub mod users;
pub mod wirings;

/// Commands restricted to `Caller::Host`.
///
/// Anything not in this list is callable by either caller kind. Reads of
/// non-sensitive state (lists, gets) are allowed for agents so the
/// container-side `cclaw` shim can introspect its own configuration.
pub const HOST_ONLY_COMMANDS: &[&str] = &[
    "groups.create",
    "groups.update",
    "groups.delete",
    "groups.restart",
    "groups.config.update",
    "groups.config.add-mcp-server",
    "groups.config.remove-mcp-server",
    "groups.config.add-package",
    "groups.config.remove-package",
    "groups.config.set-egress-allow",
    "groups.config.set-resource-limits",
    "groups.config.set-coding-enabled",
    "messaging-groups.create",
    "messaging-groups.update",
    "messaging-groups.delete",
    "wirings.create",
    "wirings.update",
    "wirings.delete",
    "users.create",
    "users.update",
    "roles.grant",
    "roles.revoke",
    "members.add",
    "members.remove",
    "destinations.add",
    "destinations.remove",
    "sessions.delete",
    "approvals.approve_sender",
    "approvals.approve",
    "approvals.deny",
    "approvals.revoke",
    "budgets.set",
    "db.backup",
    "db.restore",
    "dropped-messages.replay",
    "mcp.add",
];

/// True when `command` requires `Caller::Host`.
pub fn requires_host_caller(command: &str) -> bool {
    HOST_ONLY_COMMANDS.contains(&command)
}

/// Verify the caller passes the scope gate for `command`. Returns
/// `Err(ErrorPayload)` with code `"permission_denied"` if a non-host caller
/// tried to invoke a host-only command.
pub fn check_caller(command: &str, caller: &Caller) -> Result<(), ErrorPayload> {
    if requires_host_caller(command) && !matches!(caller, Caller::Host) {
        return Err(ErrorPayload::new(
            "permission_denied",
            format!("command `{command}` is host-only"),
        ));
    }
    Ok(())
}

/// Helper: pluck a required `String` from a JSON args object.
pub(crate) fn req_str(args: &Value, key: &str) -> Result<String, ErrorPayload> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ErrorPayload::new("bad_request", format!("missing string `{key}`")))
}

/// Helper: pluck an optional `String` from a JSON args object.
pub(crate) fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Helper: parse a uuid-or-prefixed-uuid string into a UUID.
pub(crate) fn parse_uuid(s: &str) -> Result<uuid::Uuid, ErrorPayload> {
    // Strip a `prefix_` prefix so `ag_<uuid>` and the bare uuid both work.
    let candidate = s.split_once('_').map_or(s, |(_, rest)| rest);
    uuid::Uuid::parse_str(candidate)
        .map_err(|e| ErrorPayload::new("bad_request", format!("invalid id `{s}`: {e}")))
}

/// Helper: parse `args["id"]` as an `AgentGroupId`.
pub(crate) fn parse_agent_group_id(args: &Value, key: &str) -> Result<AgentGroupId, ErrorPayload> {
    let s = req_str(args, key)?;
    Ok(AgentGroupId(parse_uuid(&s)?))
}

/// Wrap a `DbError` as an `ErrorPayload`. `NotFound` maps to a `not_found`
/// code; everything else to `db_error`.
pub(crate) fn db_err(err: copperclaw_db::DbError) -> ErrorPayload {
    match err {
        copperclaw_db::DbError::NotFound => ErrorPayload::new("not_found", "not found"),
        other => ErrorPayload::new("db_error", other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::{AgentGroupId, SessionId};

    #[test]
    fn host_only_commands_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in HOST_ONLY_COMMANDS {
            assert!(seen.insert(*c), "duplicate host-only entry: {c}");
        }
    }

    #[test]
    fn requires_host_caller_recognizes_mutations() {
        assert!(requires_host_caller("groups.create"));
        assert!(requires_host_caller("members.remove"));
        assert!(!requires_host_caller("groups.list"));
        assert!(!requires_host_caller("sessions.get"));
    }

    #[test]
    fn check_caller_host_always_allowed() {
        check_caller("groups.delete", &Caller::Host).unwrap();
        check_caller("groups.list", &Caller::Host).unwrap();
    }

    #[test]
    fn check_caller_agent_blocked_on_mutation() {
        let agent = Caller::Agent {
            session_id: SessionId::nil(),
            agent_group_id: AgentGroupId::nil(),
            messaging_group_id: None,
        };
        let err = check_caller("groups.delete", &agent).unwrap_err();
        assert_eq!(err.code, "permission_denied");
    }

    #[test]
    fn check_caller_agent_allowed_on_read() {
        let agent = Caller::Agent {
            session_id: SessionId::nil(),
            agent_group_id: AgentGroupId::nil(),
            messaging_group_id: None,
        };
        check_caller("groups.list", &agent).unwrap();
    }

    #[test]
    fn req_str_returns_value() {
        let v = serde_json::json!({"id": "foo"});
        assert_eq!(req_str(&v, "id").unwrap(), "foo");
    }

    #[test]
    fn req_str_errors_on_missing() {
        let v = serde_json::json!({});
        let err = req_str(&v, "id").unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn req_str_errors_on_wrong_type() {
        let v = serde_json::json!({"id": 5});
        let err = req_str(&v, "id").unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn opt_str_handles_present_and_missing() {
        let v = serde_json::json!({"name": "x"});
        assert_eq!(opt_str(&v, "name").as_deref(), Some("x"));
        assert!(opt_str(&v, "absent").is_none());
    }

    #[test]
    fn parse_uuid_accepts_raw_and_prefixed() {
        let id = uuid::Uuid::now_v7();
        assert_eq!(parse_uuid(&id.to_string()).unwrap(), id);
        let prefixed = format!("ag_{id}");
        assert_eq!(parse_uuid(&prefixed).unwrap(), id);
    }

    #[test]
    fn parse_uuid_rejects_garbage() {
        let err = parse_uuid("not-a-uuid").unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn parse_agent_group_id_pulls_from_args() {
        let id = AgentGroupId::new();
        let v = serde_json::json!({"id": id.as_uuid().to_string()});
        assert_eq!(parse_agent_group_id(&v, "id").unwrap(), id);
    }

    #[test]
    fn db_err_maps_notfound() {
        let p = db_err(copperclaw_db::DbError::NotFound);
        assert_eq!(p.code, "not_found");
    }

    #[test]
    fn db_err_maps_other_to_db_error() {
        // Trigger a non-NotFound error by failing to parse a uuid.
        let p = db_err(copperclaw_db::DbError::Invariant("x".into()));
        assert_eq!(p.code, "db_error");
    }
}
