//! Handlers for `wirings.*` commands.

use super::{db_err, opt_str, parse_uuid, req_str};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::messaging_group_agents;
use copperclaw_cclaw::ErrorPayload;
use copperclaw_types::{AgentGroupId, EngageMode, MessagingGroupId, SessionMode, WiringId};
use serde_json::{json, Value};

#[allow(clippy::similar_names)]
pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    // Optional `agent_group_id` or `messaging_group_id` filter.
    let filter_mg = args.get("messaging_group_id").and_then(Value::as_str);
    let filter_ag = args.get("agent_group_id").and_then(Value::as_str);
    let rows = match (filter_mg, filter_ag) {
        (Some(s), _) => {
            let id = MessagingGroupId(parse_uuid(s)?);
            messaging_group_agents::list_for_mg(central, id).map_err(db_err)?
        }
        (None, Some(s)) => {
            let id = AgentGroupId(parse_uuid(s)?);
            messaging_group_agents::list_for_ag(central, id).map_err(db_err)?
        }
        (None, None) => {
            // No filter: list across all messaging groups in the DB.
            let mgs = copperclaw_db::tables::messaging_groups::list(central).map_err(db_err)?;
            let mut combined = Vec::new();
            for mg in mgs {
                let chunk =
                    messaging_group_agents::list_for_mg(central, mg.id).map_err(db_err)?;
                combined.extend(chunk);
            }
            combined
        }
    };
    Ok(json!(rows.iter().map(wiring_to_json).collect::<Vec<_>>()))
}

pub fn get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_wiring_id(args, "id")?;
    let row = messaging_group_agents::get(central, id).map_err(db_err)?;
    Ok(wiring_to_json(&row))
}

pub fn create(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let mg_id = MessagingGroupId(parse_uuid(&req_str(args, "messaging_group_id")?)?);
    let ag_id = AgentGroupId(parse_uuid(&req_str(args, "agent_group_id")?)?);
    let engage = parse_engage_mode(&req_str(args, "engage")?)?;
    let pattern = opt_str(args, "pattern");
    let sender_scope = opt_str(args, "sender_scope").unwrap_or_else(|| "all".to_owned());
    let session_mode = match opt_str(args, "session_mode") {
        Some(s) => parse_session_mode(&s)?,
        None => SessionMode::Shared,
    };
    let ignored_message_policy = opt_str(args, "ignored_message_policy")
        .unwrap_or_else(|| "drop".to_owned());
    let priority = args
        .get("priority")
        .and_then(Value::as_i64)
        .map_or(0, |n| i32::try_from(n).unwrap_or(0));
    let row = messaging_group_agents::upsert(
        central,
        messaging_group_agents::UpsertWiring {
            messaging_group_id: mg_id,
            agent_group_id: ag_id,
            engage_mode: engage,
            engage_pattern: pattern,
            sender_scope,
            ignored_message_policy,
            session_mode,
            priority,
        },
    )
    .map_err(db_err)?;
    Ok(wiring_to_json(&row))
}

pub fn update(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_wiring_id(args, "id")?;
    let existing = messaging_group_agents::get(central, id).map_err(db_err)?;
    let engage = if let Some(s) = opt_str(args, "engage") {
        parse_engage_mode(&s)?
    } else {
        existing.engage_mode
    };
    let pattern = if args.get("pattern").is_some() {
        opt_str(args, "pattern")
    } else {
        existing.engage_pattern.clone()
    };
    let sender_scope = opt_str(args, "sender_scope").unwrap_or(existing.sender_scope.clone());
    let session_mode = if let Some(s) = opt_str(args, "session_mode") {
        parse_session_mode(&s)?
    } else {
        existing.session_mode
    };
    let ignored_message_policy = opt_str(args, "ignored_message_policy")
        .unwrap_or(existing.ignored_message_policy.clone());
    let priority = args
        .get("priority")
        .and_then(Value::as_i64)
        .map_or(existing.priority, |n| i32::try_from(n).unwrap_or(existing.priority));
    let row = messaging_group_agents::upsert(
        central,
        messaging_group_agents::UpsertWiring {
            messaging_group_id: existing.messaging_group_id,
            agent_group_id: existing.agent_group_id,
            engage_mode: engage,
            engage_pattern: pattern,
            sender_scope,
            ignored_message_policy,
            session_mode,
            priority,
        },
    )
    .map_err(db_err)?;
    Ok(wiring_to_json(&row))
}

pub fn delete(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_wiring_id(args, "id")?;
    messaging_group_agents::delete(central, id).map_err(db_err)?;
    Ok(json!({"deleted": id.as_uuid().to_string()}))
}

fn parse_wiring_id(args: &Value, key: &str) -> Result<WiringId, ErrorPayload> {
    let s = req_str(args, key)?;
    Ok(WiringId(parse_uuid(&s)?))
}

fn parse_engage_mode(s: &str) -> Result<EngageMode, ErrorPayload> {
    match s {
        "pattern" => Ok(EngageMode::Pattern),
        "mention" => Ok(EngageMode::Mention),
        "mention-sticky" => Ok(EngageMode::MentionSticky),
        other => Err(ErrorPayload::new(
            "bad_request",
            format!("unknown engage mode `{other}`"),
        )),
    }
}

fn parse_session_mode(s: &str) -> Result<SessionMode, ErrorPayload> {
    match s {
        "shared" => Ok(SessionMode::Shared),
        "per-thread" => Ok(SessionMode::PerThread),
        "agent-shared" => Ok(SessionMode::AgentShared),
        other => Err(ErrorPayload::new(
            "bad_request",
            format!("unknown session mode `{other}`"),
        )),
    }
}

fn engage_mode_str(m: EngageMode) -> &'static str {
    match m {
        EngageMode::Pattern => "pattern",
        EngageMode::Mention => "mention",
        EngageMode::MentionSticky => "mention-sticky",
    }
}

fn session_mode_str(m: SessionMode) -> &'static str {
    match m {
        SessionMode::Shared => "shared",
        SessionMode::PerThread => "per-thread",
        SessionMode::AgentShared => "agent-shared",
    }
}

fn wiring_to_json(w: &messaging_group_agents::MessagingGroupAgent) -> Value {
    json!({
        "id": w.id.as_uuid().to_string(),
        "messaging_group_id": w.messaging_group_id.as_uuid().to_string(),
        "agent_group_id": w.agent_group_id.as_uuid().to_string(),
        "engage": engage_mode_str(w.engage_mode),
        "pattern": w.engage_pattern,
        "sender_scope": w.sender_scope,
        "ignored_message_policy": w.ignored_message_policy,
        "session_mode": session_mode_str(w.session_mode),
        "priority": w.priority,
        "created_at": w.created_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use copperclaw_db::tables::messaging_groups::{upsert as upsert_mg, UpsertMessagingGroup};
    use copperclaw_types::ChannelType;

    fn db_with_mg_ag() -> (CentralDb, MessagingGroupId, AgentGroupId) {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("cli"),
                platform_id: "p1".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        (db, mg.id, ag.id)
    }

    #[test]
    fn create_minimal_args() {
        let (db, mg, ag) = db_with_mg_ag();
        let v = create(
            &json!({
                "messaging_group_id": mg.as_uuid().to_string(),
                "agent_group_id": ag.as_uuid().to_string(),
                "engage": "mention",
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["engage"], "mention");
        assert_eq!(v["sender_scope"], "all");
        assert_eq!(v["session_mode"], "shared");
        assert_eq!(v["priority"], 0);
    }

    #[test]
    fn create_with_all_options() {
        let (db, mg, ag) = db_with_mg_ag();
        let v = create(
            &json!({
                "messaging_group_id": mg.as_uuid().to_string(),
                "agent_group_id": ag.as_uuid().to_string(),
                "engage": "pattern",
                "pattern": "^hi",
                "sender_scope": "known",
                "session_mode": "per-thread",
                "priority": 5,
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["engage"], "pattern");
        assert_eq!(v["pattern"], "^hi");
        assert_eq!(v["priority"], 5);
    }

    #[test]
    fn create_rejects_unknown_engage() {
        let (db, mg, ag) = db_with_mg_ag();
        let err = create(
            &json!({
                "messaging_group_id": mg.as_uuid().to_string(),
                "agent_group_id": ag.as_uuid().to_string(),
                "engage": "loud",
            }),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn create_rejects_unknown_session_mode() {
        let (db, mg, ag) = db_with_mg_ag();
        let err = create(
            &json!({
                "messaging_group_id": mg.as_uuid().to_string(),
                "agent_group_id": ag.as_uuid().to_string(),
                "engage": "mention",
                "session_mode": "wat",
            }),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[allow(clippy::similar_names)]
    #[test]
    fn list_returns_all_then_filters() {
        let (db, mg, ag) = db_with_mg_ag();
        create(
            &json!({
                "messaging_group_id": mg.as_uuid().to_string(),
                "agent_group_id": ag.as_uuid().to_string(),
                "engage": "mention",
            }),
            &db,
        )
        .unwrap();
        let all = list(&Value::Null, &db).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 1);
        let only_mg = list(
            &json!({"messaging_group_id": mg.as_uuid().to_string()}),
            &db,
        )
        .unwrap();
        assert_eq!(only_mg.as_array().unwrap().len(), 1);
        let only_ag = list(
            &json!({"agent_group_id": ag.as_uuid().to_string()}),
            &db,
        )
        .unwrap();
        assert_eq!(only_ag.as_array().unwrap().len(), 1);
    }

    #[test]
    fn update_changes_priority() {
        let (db, mg, ag) = db_with_mg_ag();
        let v = create(
            &json!({
                "messaging_group_id": mg.as_uuid().to_string(),
                "agent_group_id": ag.as_uuid().to_string(),
                "engage": "mention",
            }),
            &db,
        )
        .unwrap();
        let id = v["id"].as_str().unwrap();
        let v = update(&json!({"id": id, "priority": 9}), &db).unwrap();
        assert_eq!(v["priority"], 9);
    }

    #[test]
    fn update_clears_pattern_when_supplied_null() {
        let (db, mg, ag) = db_with_mg_ag();
        let v = create(
            &json!({
                "messaging_group_id": mg.as_uuid().to_string(),
                "agent_group_id": ag.as_uuid().to_string(),
                "engage": "pattern",
                "pattern": "x",
            }),
            &db,
        )
        .unwrap();
        let id = v["id"].as_str().unwrap();
        let v = update(&json!({"id": id, "pattern": Value::Null}), &db).unwrap();
        assert!(v["pattern"].is_null());
    }

    #[test]
    fn update_unknown_id_errors() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = update(
            &json!({"id": uuid::Uuid::now_v7().to_string()}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn delete_then_get_is_not_found() {
        let (db, mg, ag) = db_with_mg_ag();
        let v = create(
            &json!({
                "messaging_group_id": mg.as_uuid().to_string(),
                "agent_group_id": ag.as_uuid().to_string(),
                "engage": "mention",
            }),
            &db,
        )
        .unwrap();
        let id = v["id"].as_str().unwrap();
        delete(&json!({"id": id}), &db).unwrap();
        let err = get(&json!({"id": id}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }
}
