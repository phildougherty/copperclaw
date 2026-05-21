//! Handlers for `destinations.*` commands.

use super::{db_err, opt_str, parse_uuid, req_str};
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::agent_destinations;
use ironclaw_iclaw::ErrorPayload;
use ironclaw_types::{AgentGroupId, DestinationKind};
use serde_json::{json, Value};

pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let ag = AgentGroupId(parse_uuid(&req_str(args, "agent_group_id")?)?);
    let rows = agent_destinations::list(central, ag).map_err(db_err)?;
    Ok(json!(rows.iter().map(dest_to_json).collect::<Vec<_>>()))
}

pub fn add(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let ag = AgentGroupId(parse_uuid(&req_str(args, "agent_group_id")?)?);
    let name = req_str(args, "name")?;
    let kind_str = req_str(args, "type")?;
    let kind = parse_destination_kind(&kind_str)?;
    let target_id = match kind {
        DestinationKind::Channel => {
            // We accept the channel-form variants of target identifier and
            // fall back to `platform_id` (the most common case).
            opt_str(args, "platform_id")
                .or_else(|| opt_str(args, "target_id"))
                .ok_or_else(|| {
                    ErrorPayload::new("bad_request", "channel destination needs `platform_id`")
                })?
        }
        DestinationKind::Agent => opt_str(args, "target_agent_group_id")
            .or_else(|| opt_str(args, "target_id"))
            .ok_or_else(|| {
                ErrorPayload::new("bad_request", "agent destination needs `target_agent_group_id`")
            })?,
    };
    let row = agent_destinations::add(central, ag, name, kind, target_id).map_err(db_err)?;
    Ok(dest_to_json(&row))
}

pub fn remove(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let ag = AgentGroupId(parse_uuid(&req_str(args, "agent_group_id")?)?);
    let name = req_str(args, "name")?;
    agent_destinations::remove(central, ag, &name).map_err(db_err)?;
    Ok(json!({"removed": name}))
}

fn parse_destination_kind(s: &str) -> Result<DestinationKind, ErrorPayload> {
    match s {
        "channel" => Ok(DestinationKind::Channel),
        "agent" => Ok(DestinationKind::Agent),
        other => Err(ErrorPayload::new(
            "bad_request",
            format!("unknown destination type `{other}`"),
        )),
    }
}

fn dest_to_json(d: &agent_destinations::AgentDestination) -> Value {
    json!({
        "agent_group_id": d.agent_group_id.as_uuid().to_string(),
        "local_name": d.local_name,
        "target_type": match d.target_type {
            DestinationKind::Channel => "channel",
            DestinationKind::Agent => "agent",
        },
        "target_id": d.target_id,
        "created_at": d.created_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};

    fn db_with_group() -> (CentralDb, AgentGroupId) {
        let db = CentralDb::open_in_memory().unwrap();
        let g = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        (db, g.id)
    }

    #[test]
    fn add_channel_destination() {
        let (db, g) = db_with_group();
        let v = add(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "name": "ops",
                "type": "channel",
                "platform_id": "C1",
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["target_type"], "channel");
        assert_eq!(v["target_id"], "C1");
    }

    #[test]
    fn add_agent_destination() {
        let (db, g) = db_with_group();
        let other = create_ag(
            &db,
            CreateAgentGroup {
                name: "p".into(),
                folder: "peer".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let v = add(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "name": "peer",
                "type": "agent",
                "target_agent_group_id": other.id.as_uuid().to_string(),
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["target_type"], "agent");
    }

    #[test]
    fn channel_missing_platform_id_errors() {
        let (db, g) = db_with_group();
        let err = add(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "name": "x",
                "type": "channel",
            }),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn agent_missing_target_errors() {
        let (db, g) = db_with_group();
        let err = add(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "name": "x",
                "type": "agent",
            }),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn unknown_destination_kind_errors() {
        let (db, g) = db_with_group();
        let err = add(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "name": "x",
                "type": "smoke-signal",
            }),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn list_then_remove() {
        let (db, g) = db_with_group();
        add(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "name": "n",
                "type": "channel",
                "platform_id": "p",
            }),
            &db,
        )
        .unwrap();
        let v = list(
            &json!({"agent_group_id": g.as_uuid().to_string()}),
            &db,
        )
        .unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        remove(
            &json!({"agent_group_id": g.as_uuid().to_string(), "name": "n"}),
            &db,
        )
        .unwrap();
        let v = list(
            &json!({"agent_group_id": g.as_uuid().to_string()}),
            &db,
        )
        .unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[test]
    fn remove_missing_is_not_found() {
        let (db, g) = db_with_group();
        let err = remove(
            &json!({"agent_group_id": g.as_uuid().to_string(), "name": "ghost"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }
}
