//! Handlers for `approvals.*` commands.
//!
//! ## Approval families and action strings
//!
//! Pending approvals carry an `action` string column that the generic
//! [`approve`] handler switches on to apply the per-family side effect.
//! The recognised action vocabulary matches the existing self-mod tool
//! names so a single row written by either the (current) sender flow or
//! the (forward-looking) `self_mod` approval queue dispatches correctly:
//!
//! - `"sender"` / `"approve_sender"` — upsert the row's
//!   `(channel_type, platform_id)` pair into the central `users` table.
//!   The sender-scope gate consults `users` on every inbound so the
//!   approval is effective on the next message without a host restart.
//! - `"channel"` — upsert a `messaging_groups` row keyed on
//!   `(channel_type, platform_id)` from the row's columns. No wiring is
//!   created — that is a separate operator decision (`cclaw wirings create`).
//! - `"install_packages"` — read `apt`/`npm` arrays from `payload`, merge
//!   them into `container_configs.packages_apt`/`packages_npm`. Does NOT
//!   queue a rebuild; the response includes a `rebuild_hint` field so the
//!   operator knows to run `cclaw groups restart <agent_group_id>`.
//! - `"add_mcp_server"` — read `name`/`transport` from `payload`, insert
//!   into `container_configs.mcp_servers`. Same no-auto-rebuild stance.
//!
//! ## Idempotency
//!
//! - Re-approving a row that is already `approved` returns the resolved
//!   row unchanged (no second side-effect application). This avoids
//!   double-installing packages or duplicating users-table rows.
//! - Re-denying an already-denied row likewise no-ops.
//! - Trying to approve a denied row (or deny an approved row) returns
//!   `conflict` — operators must explicitly resolve via DB if they want
//!   to reverse course.

use super::{db_err, opt_str, parse_uuid, req_str};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::pending_approvals::{ApprovalStatus, DecisionOutcome};
use copperclaw_db::tables::{container_configs, messaging_groups, pending_approvals, users};
use copperclaw_types::{AgentGroupId, ApprovalId};
use serde_json::{Value, json};

pub fn list(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    // Lapse any overdue pending rows first so the live list never includes
    // an approval that has already passed its TTL.
    pending_approvals::sweep_expired(central, chrono::Utc::now()).map_err(db_err)?;
    let rows = pending_approvals::list(central, None, None).map_err(db_err)?;
    Ok(json!(rows.iter().map(approval_to_json).collect::<Vec<_>>()))
}

pub fn get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = ApprovalId(parse_uuid(&req_str(args, "id")?)?);
    let row = pending_approvals::get(central, id).map_err(db_err)?;
    Ok(approval_to_json(&row))
}

/// Approve a sender by `(channel_type, identity)` via an upsert into
/// the central `users` table. The `ApprovalsModule`'s gate reads
/// `users` on every inbound, so the approval is effective on the
/// next message.
pub fn approve_sender(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let channel = req_str(args, "channel_type")?;
    let identity = req_str(args, "identity")?;
    if channel.is_empty() || identity.is_empty() {
        return Err(ErrorPayload::new(
            "bad_request".to_string(),
            "channel_type and identity are required and must be non-empty".to_string(),
        ));
    }
    let display_name = opt_str(args, "display_name");
    let user = users::upsert(
        central,
        users::UpsertUser {
            kind: channel.clone(),
            identity: identity.clone(),
            display_name: display_name.clone(),
        },
    )
    .map_err(db_err)?;
    Ok(json!({
        "user_id": user.id.as_uuid().to_string(),
        "channel_type": channel,
        "identity": identity,
        "display_name": display_name,
    }))
}

/// Generic approve-by-id dispatcher. Looks up the row, dispatches per
/// `action` family, applies the side effect, and marks the row
/// `status = 'approved'`. Idempotent for already-approved rows
/// (returns the resolved row unchanged).
pub fn approve(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = ApprovalId(parse_uuid(&req_str(args, "id")?)?);
    // Lapse any overdue pending rows first; this both keeps the audit log
    // honest and turns an expired-but-still-`pending` row into a deterministic
    // `expired` conflict below rather than a silently honoured late approval.
    pending_approvals::sweep_expired(central, chrono::Utc::now()).map_err(db_err)?;
    let row = pending_approvals::get(central, id).map_err(db_err)?;
    match row.status {
        ApprovalStatus::Approved => {
            // Idempotent: already applied. Return the row + the
            // family it landed in so callers can present a useful
            // message instead of an error.
            return Ok(json!({
                "approval": approval_to_json(&row),
                "applied": false,
                "reason": "already_approved",
            }));
        }
        ApprovalStatus::Denied => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is denied; refusing to approve",
            ));
        }
        ApprovalStatus::Expired => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is expired; refusing to approve",
            ));
        }
        ApprovalStatus::Revoked => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is revoked; refusing to approve",
            ));
        }
        ApprovalStatus::Pending => {}
    }

    let action = row.action.as_str();
    let side_effect = match action {
        "sender" | "approve_sender" => apply_sender(central, &row)?,
        "channel" => apply_channel(central, &row)?,
        "install_packages" => apply_install_packages(central, &row)?,
        "add_mcp_server" => apply_add_mcp_server(central, &row)?,
        other => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("unknown approval action `{other}`; cannot dispatch"),
            ));
        }
    };

    pending_approvals::update_status(central, id, ApprovalStatus::Approved).map_err(db_err)?;
    pending_approvals::record_decision(central, id, action, DecisionOutcome::Approve, "host", None)
        .map_err(db_err)?;
    let after = pending_approvals::get(central, id).map_err(db_err)?;
    Ok(json!({
        "approval": approval_to_json(&after),
        "applied": true,
        "side_effect": side_effect,
    }))
}

/// Mark a pending row `denied` without applying any side effects.
/// Idempotent for already-denied rows. Conflicts with `approved`.
pub fn deny(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = ApprovalId(parse_uuid(&req_str(args, "id")?)?);
    pending_approvals::sweep_expired(central, chrono::Utc::now()).map_err(db_err)?;
    let row = pending_approvals::get(central, id).map_err(db_err)?;
    match row.status {
        ApprovalStatus::Denied => {
            return Ok(json!({
                "approval": approval_to_json(&row),
                "applied": false,
                "reason": "already_denied",
            }));
        }
        ApprovalStatus::Approved => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is approved; refusing to deny",
            ));
        }
        ApprovalStatus::Expired => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is expired; refusing to deny",
            ));
        }
        ApprovalStatus::Revoked => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is revoked; refusing to deny",
            ));
        }
        ApprovalStatus::Pending => {}
    }
    pending_approvals::update_status(central, id, ApprovalStatus::Denied).map_err(db_err)?;
    pending_approvals::record_decision(
        central,
        id,
        row.action.as_str(),
        DecisionOutcome::Deny,
        "host",
        None,
    )
    .map_err(db_err)?;
    let after = pending_approvals::get(central, id).map_err(db_err)?;
    Ok(json!({
        "approval": approval_to_json(&after),
        "applied": true,
    }))
}

/// Revoke a pending row: withdraw the request without approving or denying
/// it. Marks the row `status = 'revoked'`, applies no side effects, and
/// appends a `revoke` decision (with the optional `reason`). Idempotent for
/// already-revoked rows. Conflicts with approved / denied / expired rows —
/// a settled decision cannot be revoked, only a live pending one.
pub fn revoke(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = ApprovalId(parse_uuid(&req_str(args, "id")?)?);
    let reason = opt_str(args, "reason");
    pending_approvals::sweep_expired(central, chrono::Utc::now()).map_err(db_err)?;
    let row = pending_approvals::get(central, id).map_err(db_err)?;
    match row.status {
        ApprovalStatus::Revoked => {
            return Ok(json!({
                "approval": approval_to_json(&row),
                "applied": false,
                "reason": "already_revoked",
            }));
        }
        ApprovalStatus::Approved => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is approved; refusing to revoke",
            ));
        }
        ApprovalStatus::Denied => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is denied; refusing to revoke",
            ));
        }
        ApprovalStatus::Expired => {
            return Err(ErrorPayload::new(
                "conflict",
                "row is expired; refusing to revoke",
            ));
        }
        ApprovalStatus::Pending => {}
    }
    pending_approvals::update_status(central, id, ApprovalStatus::Revoked).map_err(db_err)?;
    pending_approvals::record_decision(
        central,
        id,
        row.action.as_str(),
        DecisionOutcome::Revoke,
        "host",
        reason.as_deref(),
    )
    .map_err(db_err)?;
    let after = pending_approvals::get(central, id).map_err(db_err)?;
    Ok(json!({
        "approval": approval_to_json(&after),
        "applied": true,
    }))
}

/// Read the append-only approval-decision audit log. `id` scopes to one
/// approval's history; absent, returns the global log newest-first capped
/// by `limit` (default 50).
pub fn decisions(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let approval_id = match opt_str(args, "id") {
        Some(s) => Some(ApprovalId(parse_uuid(&s)?)),
        None => None,
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .unwrap_or(50);
    let rows = pending_approvals::list_decisions(central, approval_id, limit).map_err(db_err)?;
    Ok(json!(rows.iter().map(decision_to_json).collect::<Vec<_>>()))
}

// --- Per-family side-effect appliers ---------------------------------------

/// Sender family: upsert into the central `users` table. The
/// `(channel_type, platform_id)` pair comes from the row's columns;
/// optional display name is read from `payload.display_name`.
fn apply_sender(
    central: &CentralDb,
    row: &pending_approvals::PendingApproval,
) -> Result<Value, ErrorPayload> {
    let channel = row
        .channel_type
        .as_ref()
        .map(|c| c.as_str().to_owned())
        .ok_or_else(|| {
            ErrorPayload::new(
                "bad_request",
                "sender approval row is missing `channel_type`",
            )
        })?;
    let identity = row.platform_id.clone().ok_or_else(|| {
        ErrorPayload::new(
            "bad_request",
            "sender approval row is missing `platform_id`",
        )
    })?;
    let display_name = row
        .payload
        .get("display_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let user = users::upsert(
        central,
        users::UpsertUser {
            kind: channel.clone(),
            identity: identity.clone(),
            display_name: display_name.clone(),
        },
    )
    .map_err(db_err)?;
    Ok(json!({
        "kind": "sender",
        "user_id": user.id.as_uuid().to_string(),
        "channel_type": channel,
        "identity": identity,
        "display_name": display_name,
    }))
}

/// Channel family: upsert a `messaging_groups` row keyed on
/// `(channel_type, platform_id)`. Name + `is_group` are read from
/// `payload` when present. Does NOT create a wiring — that is a
/// separate operator decision (`cclaw wirings create --mg <id> --ag <id>`).
fn apply_channel(
    central: &CentralDb,
    row: &pending_approvals::PendingApproval,
) -> Result<Value, ErrorPayload> {
    let channel = row.channel_type.clone().ok_or_else(|| {
        ErrorPayload::new(
            "bad_request",
            "channel approval row is missing `channel_type`",
        )
    })?;
    let platform_id = row.platform_id.clone().ok_or_else(|| {
        ErrorPayload::new(
            "bad_request",
            "channel approval row is missing `platform_id`",
        )
    })?;
    let name = row
        .payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let is_group = row
        .payload
        .get("is_group")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let unknown_sender_policy = row
        .payload
        .get("unknown_sender_policy")
        .and_then(Value::as_str)
        .unwrap_or("strict")
        .to_owned();
    let mg = messaging_groups::upsert(
        central,
        messaging_groups::UpsertMessagingGroup {
            channel_type: channel.clone(),
            platform_id: platform_id.clone(),
            name: name.clone(),
            is_group,
            unknown_sender_policy,
        },
    )
    .map_err(db_err)?;
    Ok(json!({
        "kind": "channel",
        "messaging_group_id": mg.id.as_uuid().to_string(),
        "channel_type": channel.as_str(),
        "platform_id": platform_id,
        "name": name,
        "wiring_hint": format!(
            "messaging group created; wire it to an agent group with \
             `cclaw wirings create --mg {} --ag <ag_id> --engage <mode>`",
            mg.id.as_uuid()
        ),
    }))
}

/// `install_packages` family: merge `payload.apt`/`payload.npm` into
/// the affected group's `container_configs.packages_apt`/`packages_npm`.
/// Returns a `rebuild_hint` in the side-effect payload — does NOT queue
/// a rebuild itself.
fn apply_install_packages(
    central: &CentralDb,
    row: &pending_approvals::PendingApproval,
) -> Result<Value, ErrorPayload> {
    let ag_id = row.agent_group_id.ok_or_else(|| {
        ErrorPayload::new(
            "bad_request",
            "install_packages approval row is missing `agent_group_id`",
        )
    })?;
    let apt_new = json_str_array(&row.payload, "apt");
    let npm_new = json_str_array(&row.payload, "npm");
    if apt_new.is_empty() && npm_new.is_empty() {
        return Err(ErrorPayload::new(
            "bad_request",
            "install_packages payload has no `apt` or `npm` entries",
        ));
    }
    ensure_config_row(central, ag_id)?;
    let mut added_apt = Vec::new();
    for p in apt_new {
        container_configs::add_package_apt(central, ag_id, p.clone()).map_err(db_err)?;
        added_apt.push(p);
    }
    let mut added_npm = Vec::new();
    for p in npm_new {
        container_configs::add_package_npm(central, ag_id, p.clone()).map_err(db_err)?;
        added_npm.push(p);
    }
    Ok(json!({
        "kind": "install_packages",
        "agent_group_id": ag_id.as_uuid().to_string(),
        "added_apt": added_apt,
        "added_npm": added_npm,
        "rebuild_hint": format!(
            "packages merged into container_configs but NOT rebuilt; \
             run `cclaw groups restart {}` when convenient",
            ag_id.as_uuid()
        ),
    }))
}

/// `add_mcp_server` family: insert the row's `payload.{name,transport}`
/// into `container_configs.mcp_servers`. Replaces any existing entry
/// with the same name (mirrors the live self-mod tool behaviour).
fn apply_add_mcp_server(
    central: &CentralDb,
    row: &pending_approvals::PendingApproval,
) -> Result<Value, ErrorPayload> {
    let ag_id = row.agent_group_id.ok_or_else(|| {
        ErrorPayload::new(
            "bad_request",
            "add_mcp_server approval row is missing `agent_group_id`",
        )
    })?;
    let name = row
        .payload
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ErrorPayload::new(
                "bad_request",
                "add_mcp_server payload requires a string `name`",
            )
        })?
        .to_owned();
    if name.trim().is_empty() {
        return Err(ErrorPayload::new(
            "bad_request",
            "add_mcp_server payload `name` must be non-empty",
        ));
    }
    let transport = row.payload.get("transport").cloned().unwrap_or(Value::Null);
    ensure_config_row(central, ag_id)?;
    let mut current = container_configs::get_mcp_servers(central, ag_id)
        .map_err(db_err)
        .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
    if !current.is_object() {
        current = Value::Object(serde_json::Map::new());
    }
    if let Some(obj) = current.as_object_mut() {
        obj.insert(name.clone(), transport.clone());
    }
    container_configs::set_mcp_servers(central, ag_id, current).map_err(db_err)?;
    Ok(json!({
        "kind": "add_mcp_server",
        "agent_group_id": ag_id.as_uuid().to_string(),
        "name": name,
        "transport": transport,
        "rebuild_hint": format!(
            "mcp server merged into container_configs but NOT rebuilt; \
             run `cclaw groups restart {}` when convenient",
            ag_id.as_uuid()
        ),
    }))
}

/// Helper: read `payload[key]` as an array of non-empty strings.
fn json_str_array(payload: &Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Make sure a `container_configs` row exists for `ag_id`. Mirrors the
/// helper of the same name in `handlers::groups` — duplicated here so
/// the approvals module doesn't reach into a sibling handler's private
/// surface. Inserts a defaults-only row when missing.
fn ensure_config_row(central: &CentralDb, ag_id: AgentGroupId) -> Result<(), ErrorPayload> {
    use container_configs::{CliScope, SkillsSelector, UpsertContainerConfig};
    if container_configs::get(central, ag_id)
        .map_err(db_err)?
        .is_some()
    {
        return Ok(());
    }
    container_configs::upsert(
        central,
        UpsertContainerConfig {
            agent_group_id: ag_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: SkillsSelector::All,
            mcp_servers: Value::Object(serde_json::Map::new()),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: Value::Object(serde_json::Map::new()),
            cli_scope: CliScope::Disabled,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: Value::Object(serde_json::Map::new()),
            coding_enabled: false,
            surface_thinking: false,
        },
    )
    .map_err(db_err)?;
    Ok(())
}

fn decision_to_json(d: &pending_approvals::ApprovalDecision) -> Value {
    json!({
        "id": d.id,
        "approval_id": d.approval_id.as_uuid().to_string(),
        "action": d.action,
        "outcome": d.outcome.as_str(),
        "decided_by": d.decided_by,
        "reason": d.reason,
        "decided_at": d.decided_at.to_rfc3339(),
    })
}

fn approval_to_json(a: &pending_approvals::PendingApproval) -> Value {
    json!({
        "approval_id": a.approval_id.as_uuid().to_string(),
        "session_id": a.session_id.map(|s| s.as_uuid().to_string()),
        "request_id": a.request_id,
        "action": a.action,
        "payload": a.payload,
        "agent_group_id": a.agent_group_id.map(|g| g.as_uuid().to_string()),
        "channel_type": a.channel_type.as_ref().map(|c| c.as_str().to_owned()),
        "platform_id": a.platform_id,
        "platform_message_id": a.platform_message_id,
        "expires_at": a.expires_at.map(|t| t.to_rfc3339()),
        "status": a.status.as_str(),
        "title": a.title,
        "options": a.options,
        "created_at": a.created_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::pending_approvals::{UpsertPendingApproval, upsert};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn list_empty() {
        let db = db();
        let v = list(&Value::Null, &db).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[test]
    fn list_after_insert() {
        let db = db();
        upsert(
            &db,
            UpsertPendingApproval {
                request_id: "r1".into(),
                action: "send".into(),
                payload: json!({}),
                title: "Approve?".into(),
                options: vec!["yes".into(), "no".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn get_returns_row() {
        let db = db();
        let a = upsert(
            &db,
            UpsertPendingApproval {
                request_id: "r1".into(),
                action: "send".into(),
                payload: json!({}),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        let v = get(&json!({"id": a.approval_id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["request_id"], "r1");
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = db();
        let err = get(&json!({"id": uuid::Uuid::now_v7().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    // -----------------------------------------------------------------------
    // Generic approve / deny per-family tests
    // -----------------------------------------------------------------------

    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::pending_approvals::{ApprovalStatus, get as get_row};
    use copperclaw_types::ChannelType;

    fn seed_ag(db: &CentralDb) -> copperclaw_types::AgentGroupId {
        create_ag(
            db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    fn insert_pending(
        db: &CentralDb,
        action: &str,
        req: UpsertPendingApproval,
    ) -> copperclaw_types::ApprovalId {
        let row = upsert(
            db,
            UpsertPendingApproval {
                action: action.to_owned(),
                ..req
            },
        )
        .unwrap();
        row.approval_id
    }

    #[test]
    fn approve_unknown_id_is_not_found() {
        let db = db();
        let err = approve(&json!({"id": uuid::Uuid::now_v7().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn deny_unknown_id_is_not_found() {
        let db = db();
        let err = deny(&json!({"id": uuid::Uuid::now_v7().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn approve_unknown_action_is_bad_request() {
        let db = db();
        let id = insert_pending(
            &db,
            "totally_made_up",
            UpsertPendingApproval {
                request_id: "r-x".into(),
                payload: json!({}),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
        // Row left pending — the dispatch failed before update_status.
        let still = get_row(&db, id).unwrap();
        assert_eq!(still.status, ApprovalStatus::Pending);
    }

    #[test]
    fn approve_sender_family_upserts_user_and_resolves_row() {
        use copperclaw_db::tables::users;
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-s".into(),
                payload: json!({"display_name": "alice"}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("u-42".into()),
                title: "approve sender?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["side_effect"]["kind"], "sender");
        assert_eq!(v["side_effect"]["channel_type"], "telegram");
        assert_eq!(v["side_effect"]["identity"], "u-42");
        assert_eq!(v["side_effect"]["display_name"], "alice");
        // User row was created.
        let user = users::get_by_identity(&db, "telegram", "u-42")
            .unwrap()
            .unwrap();
        assert_eq!(user.display_name.as_deref(), Some("alice"));
        // Row status flipped.
        let after = get_row(&db, id).unwrap();
        assert_eq!(after.status, ApprovalStatus::Approved);
    }

    #[test]
    fn approve_channel_family_creates_messaging_group_no_wiring() {
        use copperclaw_db::tables::messaging_groups;
        let db = db();
        let id = insert_pending(
            &db,
            "channel",
            UpsertPendingApproval {
                request_id: "r-c".into(),
                payload: json!({"name": "demo", "is_group": true}),
                channel_type: Some(ChannelType::new("slack")),
                platform_id: Some("C-DEMO".into()),
                title: "approve channel?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["side_effect"]["kind"], "channel");
        let mg_id = v["side_effect"]["messaging_group_id"].as_str().unwrap();
        assert!(
            v["side_effect"]["wiring_hint"]
                .as_str()
                .unwrap()
                .contains("cclaw wirings create")
        );
        // The messaging_groups row exists with the right shape.
        let ct = ChannelType::new("slack");
        let mg = messaging_groups::get_by_platform(&db, &ct, "C-DEMO")
            .unwrap()
            .unwrap();
        assert_eq!(mg.id.as_uuid().to_string(), mg_id);
        assert_eq!(mg.name.as_deref(), Some("demo"));
        assert!(mg.is_group);
        // No messaging_group_agents row got created (no auto-wiring).
        let conn = db.conn().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messaging_group_agents WHERE messaging_group_id = ?1",
                rusqlite::params![mg.id.as_uuid().to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "approve channel must not auto-wire");
    }

    #[test]
    fn approve_install_packages_merges_into_container_config() {
        use copperclaw_db::tables::container_configs;
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "install_packages",
            UpsertPendingApproval {
                request_id: "r-ip".into(),
                payload: json!({"apt": ["jq", "ripgrep"], "npm": ["typescript"]}),
                agent_group_id: Some(ag),
                title: "install?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["side_effect"]["kind"], "install_packages");
        let hint = v["side_effect"]["rebuild_hint"].as_str().unwrap();
        assert!(hint.contains("cclaw groups restart"));
        // Packages landed in container_configs.
        let cfg = container_configs::get(&db, ag).unwrap().unwrap();
        assert!(cfg.packages_apt.contains(&"jq".to_string()));
        assert!(cfg.packages_apt.contains(&"ripgrep".to_string()));
        assert!(cfg.packages_npm.contains(&"typescript".to_string()));
    }

    #[test]
    fn approve_install_packages_rejects_empty_payload() {
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "install_packages",
            UpsertPendingApproval {
                request_id: "r-ip-empty".into(),
                payload: json!({}),
                agent_group_id: Some(ag),
                title: "install?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn approve_add_mcp_server_inserts_into_container_config() {
        use copperclaw_db::tables::container_configs;
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "add_mcp_server",
            UpsertPendingApproval {
                request_id: "r-mcp".into(),
                payload: json!({
                    "name": "linear",
                    "transport": {"command": "npx", "args": ["-y", "@linear/mcp"]},
                }),
                agent_group_id: Some(ag),
                title: "add mcp?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["side_effect"]["kind"], "add_mcp_server");
        assert_eq!(v["side_effect"]["name"], "linear");
        // Server landed in container_configs.mcp_servers.
        let servers = container_configs::get_mcp_servers(&db, ag).unwrap();
        let linear = servers.get("linear").unwrap();
        assert_eq!(linear["command"], "npx");
    }

    #[test]
    fn approve_add_mcp_server_requires_name() {
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "add_mcp_server",
            UpsertPendingApproval {
                request_id: "r-mcp-bad".into(),
                payload: json!({"transport": {}}),
                agent_group_id: Some(ag),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn double_approve_is_idempotent_and_does_not_reapply() {
        use copperclaw_db::tables::container_configs;
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "install_packages",
            UpsertPendingApproval {
                request_id: "r-ip-idem".into(),
                payload: json!({"apt": ["jq"]}),
                agent_group_id: Some(ag),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let first = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(first["applied"], true);
        // Second call: should NOT re-apply (no duplicate jq, no error).
        let second = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(second["applied"], false);
        assert_eq!(second["reason"], "already_approved");
        let cfg = container_configs::get(&db, ag).unwrap().unwrap();
        // Even if we called add_package_apt twice, it dedups, so we have to
        // check by counting occurrences explicitly.
        let count = cfg.packages_apt.iter().filter(|p| *p == "jq").count();
        assert_eq!(
            count, 1,
            "double approve must not duplicate the package entry"
        );
    }

    #[test]
    fn deny_marks_row_denied_without_side_effects() {
        use copperclaw_db::tables::users;
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-deny".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("slack")),
                platform_id: Some("U-DENY".into()),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = deny(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        let after = get_row(&db, id).unwrap();
        assert_eq!(after.status, ApprovalStatus::Denied);
        // Crucially: no user row was created.
        assert!(
            users::get_by_identity(&db, "slack", "U-DENY")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn double_deny_is_idempotent() {
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-deny-idem".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("slack")),
                platform_id: Some("U-X".into()),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let _ = deny(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        let second = deny(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(second["applied"], false);
        assert_eq!(second["reason"], "already_denied");
    }

    #[test]
    fn deny_after_approve_is_conflict() {
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "install_packages",
            UpsertPendingApproval {
                request_id: "r-conflict".into(),
                payload: json!({"apt": ["jq"]}),
                agent_group_id: Some(ag),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        let err = deny(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "conflict");
    }

    #[test]
    fn approve_after_deny_is_conflict() {
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-conflict2".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("u-99".into()),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        deny(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "conflict");
    }

    #[test]
    fn approve_sender_legacy_action_name_also_works() {
        // Rows landed by the existing (pre-generic) flow may carry
        // action="approve_sender" instead of "sender". Both must route.
        use copperclaw_db::tables::users;
        let db = db();
        let id = insert_pending(
            &db,
            "approve_sender",
            UpsertPendingApproval {
                request_id: "r-legacy".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("discord")),
                platform_id: Some("D-7".into()),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["side_effect"]["kind"], "sender");
        assert!(
            users::get_by_identity(&db, "discord", "D-7")
                .unwrap()
                .is_some()
        );
    }

    // -----------------------------------------------------------------------
    // Lifecycle: expiry, revocation, decision audit
    // -----------------------------------------------------------------------

    use copperclaw_db::tables::pending_approvals::DecisionOutcome;

    #[test]
    fn approve_expired_row_is_conflict_and_logs_expire() {
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-exp".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("u-exp".into()),
                // Already past its deadline.
                expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(5)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        // approve() sweeps overdue rows first, so this lapses then conflicts.
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "conflict");
        let after = get_row(&db, id).unwrap();
        assert_eq!(after.status, ApprovalStatus::Expired);
        // The sweep recorded an `expire` decision.
        let decs = pending_approvals::list_decisions(&db, Some(id), 10).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].outcome, DecisionOutcome::Expire);
        assert_eq!(decs[0].decided_by, "system:expiry");
    }

    #[test]
    fn list_sweeps_expired_rows_out_of_the_live_set() {
        let db = db();
        insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-stale".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("slack")),
                platform_id: Some("U-stale".into()),
                expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = list(&Value::Null, &db).unwrap();
        let arr = v.as_array().unwrap();
        // The stale row was swept to `expired`; it's no longer pending.
        assert!(arr.iter().all(|r| r["status"] != "pending"));
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["status"], "expired");
    }

    #[test]
    fn approve_then_record_decision_is_logged() {
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-aud".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("u-aud".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        let decs = pending_approvals::list_decisions(&db, Some(id), 10).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].outcome, DecisionOutcome::Approve);
        assert_eq!(decs[0].decided_by, "host");
    }

    #[test]
    fn revoke_marks_revoked_and_logs_decision() {
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-rev".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("slack")),
                platform_id: Some("U-rev".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = revoke(
            &json!({"id": id.as_uuid().to_string(), "reason": "operator withdrew"}),
            &db,
        )
        .unwrap();
        assert_eq!(v["applied"], true);
        let after = get_row(&db, id).unwrap();
        assert_eq!(after.status, ApprovalStatus::Revoked);
        let decs = pending_approvals::list_decisions(&db, Some(id), 10).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].outcome, DecisionOutcome::Revoke);
        assert_eq!(decs[0].reason.as_deref(), Some("operator withdrew"));
        // No side effect: no user row created.
        assert!(
            users::get_by_identity(&db, "slack", "U-rev")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn double_revoke_is_idempotent() {
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-rev-idem".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("slack")),
                platform_id: Some("U-x".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        revoke(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        let second = revoke(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(second["applied"], false);
        assert_eq!(second["reason"], "already_revoked");
        // Only one revoke decision logged.
        let decs = pending_approvals::list_decisions(&db, Some(id), 10).unwrap();
        assert_eq!(decs.len(), 1);
    }

    #[test]
    fn revoke_after_approve_is_conflict() {
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-rev-conflict".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("u-rc".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        let err = revoke(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "conflict");
    }

    #[test]
    fn revoke_unknown_id_is_not_found() {
        let db = db();
        let err = revoke(&json!({"id": uuid::Uuid::now_v7().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn decisions_handler_lists_global_and_scoped() {
        let db = db();
        let id = insert_pending(
            &db,
            "sender",
            UpsertPendingApproval {
                request_id: "r-dec-h".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("slack")),
                platform_id: Some("U-dec".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        deny(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        // Global log.
        let global = decisions(&Value::Null, &db).unwrap();
        assert_eq!(global.as_array().unwrap().len(), 1);
        assert_eq!(global[0]["outcome"], "deny");
        // Scoped to this approval.
        let scoped = decisions(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(scoped.as_array().unwrap().len(), 1);
        assert_eq!(scoped[0]["approval_id"], id.as_uuid().to_string());
    }

    #[test]
    fn approve_install_packages_missing_agent_group_is_bad_request() {
        let db = db();
        let id = insert_pending(
            &db,
            "install_packages",
            UpsertPendingApproval {
                request_id: "r-noag".into(),
                payload: json!({"apt": ["jq"]}),
                agent_group_id: None,
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }
}
