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
//! - `"save_skill"` (M19 A4) — validate the agent-authored `SKILL.md` in
//!   `payload.content` and write it under the host-computed
//!   `payload.dest_dir` (`<groups_dir>/<ag>/skills/<name>`), enforcing the
//!   frontmatter/name rules and containment under `payload.allowed_root`. The
//!   next container spawn discovers it; no rebuild is required.
//! - `"credentialed_external_action"` — grant one credentialed external action
//!   on a taint-blocked or outward-facing request (M18 V5: the public-tunnel
//!   broker's `expose`). Approving flips the row to `approved`; the requester
//!   (e.g. `TunnelBroker::expose` on the agent's retry) consults that approved
//!   grant and only then performs the action. The apply arm itself validates
//!   the payload and records the grant — it never performs the external action,
//!   so this dispatcher stays a pure DB mutation.
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
use copperclaw_modules::{DeliveryDispatcher, DispatchTarget};
use copperclaw_types::{AgentGroupId, ApprovalId};
use serde_json::{Value, json};
use std::sync::Arc;

/// Terminal text stamped onto an approval card whose TTL lapsed before anyone
/// resolved it (F3c). Kept short and actionable — the request is dead, so the
/// only useful next step is to re-ask the agent.
pub const EXPIRED_CARD_TEXT: &str =
    "This approval request expired before anyone responded. Ask the agent to try again.";

/// Sweep overdue pending approvals to `expired` AND stamp each lapsed card with
/// a terminal "expired" edit, so a silently-lapsed approval never lingers in
/// chat with live Approve/Deny buttons (F3c — "silent expiry").
///
/// Best-effort and idempotent: a card is edited exactly once (only for the rows
/// this call actually flips from `pending` to `expired`), and a row without a
/// recorded `platform_message_id` + channel coordinates is swept but not edited
/// (there is no delivered card to stamp). Returns the number of cards edited.
///
/// Wired onto the in-chat approval interceptor (`approval_intercept.rs`) so any
/// approval tap opportunistically clears stale cards on the surface that
/// already holds the dispatcher; it is equally safe to call from a periodic
/// host sweep (a lane-H follow-up) since the DB sweep is race-safe.
#[must_use]
pub fn expire_and_edit_cards(
    central: &CentralDb,
    dispatcher: &Arc<dyn DeliveryDispatcher>,
) -> usize {
    let now = chrono::Utc::now();
    // Snapshot the cards about to lapse BEFORE the sweep flips them — after the
    // flip we would have lost nothing (the row keeps its columns) but the
    // snapshot also lets us skip the sweep entirely in the common no-op case.
    let doomed: Vec<pending_approvals::PendingApproval> =
        match pending_approvals::list(central, None, Some(ApprovalStatus::Pending)) {
            Ok(rows) => rows.into_iter().filter(|r| r.is_expired_at(now)).collect(),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "approvals: could not list pending rows for expiry sweep"
                );
                return 0;
            }
        };
    if doomed.is_empty() {
        return 0;
    }
    let swept = match pending_approvals::sweep_expired(central, now) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::warn!(?err, "approvals: expiry sweep failed");
            return 0;
        }
    };
    let mut edited = 0;
    for row in doomed {
        // Only stamp rows this sweep actually flipped (guards against a
        // concurrent resolve landing between the snapshot and the sweep).
        if !swept.contains(&row.approval_id) {
            continue;
        }
        let (Some(channel), Some(platform), Some(message_id)) = (
            row.channel_type.clone(),
            row.platform_id.clone(),
            row.platform_message_id.clone(),
        ) else {
            continue; // no delivered card to stamp terminal
        };
        // The card's originating thread is not persisted on the row; edits key
        // off (channel, platform_id, message_id), which is sufficient on every
        // edit-capable adapter.
        let target = DispatchTarget::channel(channel, platform, None);
        dispatcher.edit_message(&target, &message_id, EXPIRED_CARD_TEXT);
        copperclaw_metrics::inc_approval_tap("expired_card");
        tracing::info!(
            approval_id = %row.approval_id.as_uuid(),
            "approvals: stamped expired approval card terminal"
        );
        edited += 1;
    }
    edited
}

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
/// (returns the resolved row unchanged). The CLI socket handler records
/// `decided_by = "host"`; the in-chat approvals interceptor (M18 G1) calls
/// [`resolve_approve`] directly with the tapping approver's identity so the
/// audit trail names the human who approved from chat.
pub fn approve(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = ApprovalId(parse_uuid(&req_str(args, "id")?)?);
    resolve_approve(central, id, "host")
}

/// Resolve an approval by id, recording `decided_by` on the decision log.
/// Shared by the CLI `approve` handler (`decided_by = "host"`) and the in-chat
/// approvals interceptor (`decided_by = <approver display name>`). Both routes
/// therefore write to the SAME `pending_approvals` row + `approval_decisions`
/// log, so the in-memory and DB views can never diverge, and a CLI/in-chat
/// race resolves once (first wins; the second sees `Approved` and returns
/// `applied = false`).
pub fn resolve_approve(
    central: &CentralDb,
    id: ApprovalId,
    decided_by: &str,
) -> Result<Value, ErrorPayload> {
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
        // M19 A4: write the agent-authored skill into the group's per-group
        // skills override dir. Validates (frontmatter + name==dir) and enforces
        // containment before the write; the next container spawn discovers it.
        "save_skill" => apply_save_skill(&row)?,
        // M18 V2: flip the group's `preview_enabled` master switch — the same
        // effect as `cclaw groups config update --field preview_enabled=true`.
        // The pending row is raised host-side by the preview manager when the
        // agent hits `PreviewError::Disabled`.
        "enable_preview" => {
            copperclaw_metrics::inc_preview_enable_card("approved");
            apply_enable_preview(central, &row)?
        }
        // M18 V5: the REAL applier for a credentialed external action. Approving
        // records the grant (the row flips to `approved` below); the requester —
        // e.g. the public-tunnel broker on the agent's retry — consults that
        // approved grant and only then performs the action. This arm never
        // performs the action itself, so the dispatcher stays a pure DB mutation.
        "credentialed_external_action" => apply_credentialed_external_action(&row),
        // Explicit refusal arm (M18 G1). `one_cli` (Agent Vault credential
        // grants) is never applied via this generic dispatcher; approving one
        // here would silently no-op, which is worse than a clear refusal. We
        // refuse rather than fall through to the `other` arm so the message is
        // specific and the row is left `pending` (never a silent success).
        "one_cli" => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!(
                    "approval action `{action}` cannot be resolved through the generic \
                     dispatcher yet; no side-effect applier is wired for it"
                ),
            ));
        }
        other => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("unknown approval action `{other}`; cannot dispatch"),
            ));
        }
    };

    pending_approvals::update_status(central, id, ApprovalStatus::Approved).map_err(db_err)?;
    pending_approvals::record_decision(
        central,
        id,
        action,
        DecisionOutcome::Approve,
        decided_by,
        None,
    )
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
/// CLI route records `decided_by = "host"`; see [`resolve_deny`] for the
/// in-chat variant.
pub fn deny(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = ApprovalId(parse_uuid(&req_str(args, "id")?)?);
    resolve_deny(central, id, "host")
}

/// Deny an approval by id, recording `decided_by` on the decision log. Shared
/// by the CLI `deny` handler and the in-chat approvals interceptor (M18 G1);
/// see [`resolve_approve`] for the approve twin and the race-safety contract.
pub fn resolve_deny(
    central: &CentralDb,
    id: ApprovalId,
    decided_by: &str,
) -> Result<Value, ErrorPayload> {
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
    if row.action.as_str() == "enable_preview" {
        copperclaw_metrics::inc_preview_enable_card("denied");
    }
    pending_approvals::update_status(central, id, ApprovalStatus::Denied).map_err(db_err)?;
    pending_approvals::record_decision(
        central,
        id,
        row.action.as_str(),
        DecisionOutcome::Deny,
        decided_by,
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
///
/// M18 E1: the payload may carry a `scope` (`"image"` | `"session"`). The
/// apt/npm merge is scope-independent — it always feeds the NEXT image build
/// ("works later"). Under `"session"` the agent's `install_packages` tool has
/// ALREADY run the local pip/npm install into the session's `/data` in-container
/// ("works now"), so the side-effect payload records the scope and reflects
/// that the session copy is already live (nothing here can re-run an
/// in-container install — the runtime exposes no host→container exec).
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
    let scope = row
        .payload
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("image")
        .to_owned();
    let session_scope = scope == "session";
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
    let session_note = if session_scope {
        // npm is baked here; the session copy under /data is already live from
        // the tool's in-container install, so a rebuild is optional (not urgent).
        format!(
            "session-scope: npm already installed live under /data for the current \
             session; merged into container_configs so a fresh session/group inherits \
             it. Rebuild when convenient: `cclaw groups restart {}`",
            ag_id.as_uuid()
        )
    } else {
        format!(
            "packages merged into container_configs but NOT rebuilt; \
             run `cclaw groups restart {}` when convenient",
            ag_id.as_uuid()
        )
    };
    Ok(json!({
        "kind": "install_packages",
        "agent_group_id": ag_id.as_uuid().to_string(),
        "scope": scope,
        "added_apt": added_apt,
        "added_npm": added_npm,
        "rebuild_hint": session_note,
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
    // `__preview` is the reserved session-preview relay name — an external
    // server registered under it could impersonate the preview broker. Refuse
    // even through the approval path (the request name is agent-supplied).
    if name == copperclaw_host_delivery::PREVIEW_SERVER {
        return Err(ErrorPayload::new(
            "bad_request",
            format!(
                "`{}` is a reserved server name (session-preview relay) and cannot be used for an external MCP server",
                copperclaw_host_delivery::PREVIEW_SERVER
            ),
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

/// `save_skill` family (M19 A4): validate and write an agent-authored skill
/// into the group's per-group skills override directory. The pending row was
/// raised by the delivery service, which stamped the host-computed
/// `dest_dir` (`<groups_dir>/<ag>/skills`) and `allowed_root` (`<groups_dir>`)
/// into the payload alongside the agent-supplied `name`/`content`. Approving
/// validates the frontmatter + `name == dir` invariant and enforces
/// containment (canonical dest under `allowed_root`) before writing
/// `<dest_dir>/<name>/SKILL.md`. The next container spawn's skill scan
/// discovers it — closing the write→discovery loop. No rebuild is needed
/// (skills are read fresh at spawn, not baked into the image).
///
/// A4 metric wish: `copperclaw_skills_saved_total{outcome}` (saved / rejected)
/// — recorded here and in the delivery raise path once the metrics crate gains
/// the counter (out of scope for this card, which must not touch
/// `copperclaw-metrics`).
fn apply_save_skill(row: &pending_approvals::PendingApproval) -> Result<Value, ErrorPayload> {
    let bad = |msg: &str| ErrorPayload::new("bad_request", msg.to_string());
    let name = row
        .payload
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("save_skill payload requires a string `name`"))?;
    let content = row
        .payload
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("save_skill payload requires a string `content`"))?;
    let dest_dir = row
        .payload
        .get("dest_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("save_skill payload requires a string `dest_dir`"))?;
    let allowed_root = row
        .payload
        .get("allowed_root")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("save_skill payload requires a string `allowed_root`"))?;

    let dest = std::path::PathBuf::from(dest_dir);
    let roots = [std::path::PathBuf::from(allowed_root)];
    let written = copperclaw_skills::save_group_skill(&dest, &roots, name, content)
        // A well-formed but invalid skill (bad frontmatter, name mismatch,
        // containment escape) surfaces the precise skills-crate error.
        .map_err(|e| ErrorPayload::new("bad_request", format!("save_skill rejected: {e}")))?;

    Ok(json!({
        "kind": "save_skill",
        "agent_group_id": row.agent_group_id.map(|g| g.as_uuid().to_string()),
        "name": name,
        "path": written.to_string_lossy(),
        "note": "skill saved to the group's skills override; it is discovered and \
                 available on the agent's NEXT session (no rebuild needed)",
    }))
}

/// `enable_preview` family (M18 V2): flip the group's `preview_enabled`
/// master switch on. Effect-identical to the operator running
/// `cclaw groups config update --field preview_enabled=true <ag>` — the
/// preview manager consults `container_configs.preview_enabled` on the next
/// `expose_preview`, so no rebuild/restart is required for it to take effect.
/// A defaults-only config row is created first when the group has none (the
/// "no config row" case is one of the two ways `PreviewError::Disabled` is
/// raised), so the narrow `set_preview_enabled` setter always has a row to
/// update.
fn apply_enable_preview(
    central: &CentralDb,
    row: &pending_approvals::PendingApproval,
) -> Result<Value, ErrorPayload> {
    let ag_id = row.agent_group_id.ok_or_else(|| {
        ErrorPayload::new(
            "bad_request",
            "enable_preview approval row is missing `agent_group_id`",
        )
    })?;
    ensure_config_row(central, ag_id)?;
    container_configs::set_preview_enabled(central, ag_id, true).map_err(db_err)?;
    Ok(json!({
        "kind": "enable_preview",
        "agent_group_id": ag_id.as_uuid().to_string(),
        "preview_enabled": true,
        "note": "previews enabled for this group; the agent can retry `expose_preview` now",
    }))
}

/// `credentialed_external_action` family (M18 V5): record the grant. Approving
/// authorizes ONE credentialed external action for the requester that raised the
/// row (the public-tunnel broker's `expose`, or a future taint-clearing arm).
/// The grant IS the row flipping to `approved` (done by the generic dispatcher
/// after this returns); the requester consults that on its next attempt. This
/// applier never performs the external action itself — it validates the payload
/// and echoes an operator-legible side-effect so the approve response and audit
/// name exactly what was authorized. Infallible: any well-formed approval row
/// can be granted (a missing/partial payload just yields a sparser echo).
fn apply_credentialed_external_action(row: &pending_approvals::PendingApproval) -> Value {
    // The V5 tunnel broker stamps `kind = "tunnel"` plus the exposure specifics.
    // Unknown/absent kinds still resolve — the grant semantics are the same.
    let action_kind = row
        .payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("credentialed_external_action");
    json!({
        "kind": "credentialed_external_action",
        "action_kind": action_kind,
        "agent_group_id": row.agent_group_id.map(|g| g.as_uuid().to_string()),
        "session_id": row.session_id.map(|s| s.as_uuid().to_string()),
        "host_port": row.payload.get("host_port").cloned().unwrap_or(Value::Null),
        "upstream": row.payload.get("upstream").cloned().unwrap_or(Value::Null),
        "note": "authorization granted for one credentialed external action; the \
                 requester (e.g. the public-tunnel broker) consults this approved \
                 grant before acting. No external action was performed by this approval.",
    })
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
            tool_profile: None,
            preview_enabled: false,
            preview_bind: None,
            check_command: None,
            verify_gate: true,
            image_profile: copperclaw_types::ImageProfile::Minimal,
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
    fn approve_install_packages_session_scope_still_merges_bakeables() {
        // M18 E1: a session-scope request (npm already installed live
        // in-container) must STILL merge the bakeable npm into the config so a
        // fresh session/group inherits it. The side-effect echoes the scope
        // and a session-flavoured rebuild note.
        use copperclaw_db::tables::container_configs;
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "install_packages",
            UpsertPendingApproval {
                request_id: "r-ip-session".into(),
                payload: json!({"apt": [], "npm": ["typescript"], "scope": "session"}),
                agent_group_id: Some(ag),
                title: "install?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["side_effect"]["kind"], "install_packages");
        assert_eq!(v["side_effect"]["scope"], "session");
        let hint = v["side_effect"]["rebuild_hint"].as_str().unwrap();
        assert!(hint.contains("session-scope"));
        let cfg = container_configs::get(&db, ag).unwrap().unwrap();
        assert!(cfg.packages_npm.contains(&"typescript".to_string()));
    }

    // -----------------------------------------------------------------------
    // M19 A4: save_skill approval → validate → write → discovery
    // -----------------------------------------------------------------------

    const A4_SKILL: &str =
        "---\nname: my-skill\ndescription: A reusable procedure\n---\n# Steps\ndo it\n";

    /// End-to-end: an approved `save_skill` writes the SKILL.md into the
    /// group's per-group skills override dir, and a fresh `SkillRegistry::scan`
    /// (what the next container spawn does) discovers and exposes it.
    #[test]
    fn approve_save_skill_writes_and_is_discovered_on_next_spawn() {
        let db = db();
        let ag = seed_ag(&db);
        let td = tempfile::tempdir().unwrap();
        let groups_dir = td.path().join("groups");
        let dest_dir = groups_dir.join(ag.as_uuid().to_string()).join("skills");
        let id = insert_pending(
            &db,
            "save_skill",
            UpsertPendingApproval {
                request_id: format!("save-skill:{}:my-skill", ag.as_uuid()),
                payload: json!({
                    "name": "my-skill",
                    "content": A4_SKILL,
                    "reason": "handy",
                    "dest_dir": dest_dir.to_string_lossy(),
                    "allowed_root": groups_dir.to_string_lossy(),
                }),
                agent_group_id: Some(ag),
                title: "Save skill: my-skill".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["side_effect"]["kind"], "save_skill");
        assert_eq!(v["side_effect"]["name"], "my-skill");

        // The SKILL.md landed on disk.
        let skill_md = dest_dir.join("my-skill").join("SKILL.md");
        assert!(skill_md.is_file(), "SKILL.md should exist at {skill_md:?}");

        // The next spawn's discovery scan finds it as a group-sourced skill.
        let global = td.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        let reg = copperclaw_skills::SkillRegistry::scan(&global, Some((ag, &dest_dir))).unwrap();
        let skill = reg.get("my-skill").expect("saved skill discovered");
        assert_eq!(skill.description, "A reusable procedure");
        assert_eq!(skill.source, copperclaw_skills::SkillSource::Group(ag));
    }

    /// Invalid frontmatter is refused at approve time with the precise
    /// skills-crate validation error (defense-in-depth: the tool refuses it
    /// first, but the write boundary re-validates).
    #[test]
    fn approve_save_skill_rejects_invalid_frontmatter() {
        let db = db();
        let ag = seed_ag(&db);
        let td = tempfile::tempdir().unwrap();
        let groups_dir = td.path().join("groups");
        let dest_dir = groups_dir.join(ag.as_uuid().to_string()).join("skills");
        let id = insert_pending(
            &db,
            "save_skill",
            UpsertPendingApproval {
                request_id: "r-bad-skill".into(),
                payload: json!({
                    "name": "my-skill",
                    "content": "no frontmatter here\n",
                    "reason": "x",
                    "dest_dir": dest_dir.to_string_lossy(),
                    "allowed_root": groups_dir.to_string_lossy(),
                }),
                agent_group_id: Some(ag),
                title: "Save skill".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("rejected"), "got: {}", err.message);
        // Nothing was written and the row stays pending (not silently approved).
        assert!(!dest_dir.join("my-skill").exists());
        let row = get_row(&db, id).unwrap();
        assert_eq!(row.status, ApprovalStatus::Pending);
    }

    /// A destination that canonically escapes the allowed root is refused —
    /// the containment guard reused from the skills crate.
    #[test]
    fn approve_save_skill_rejects_containment_escape() {
        let db = db();
        let ag = seed_ag(&db);
        let td = tempfile::tempdir().unwrap();
        // allowed_root is a sibling of the destination — the write escapes it.
        let allowed = td.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let dest_dir = td.path().join("outside").join("skills");
        let id = insert_pending(
            &db,
            "save_skill",
            UpsertPendingApproval {
                request_id: "r-escape-skill".into(),
                payload: json!({
                    "name": "my-skill",
                    "content": A4_SKILL,
                    "reason": "x",
                    "dest_dir": dest_dir.to_string_lossy(),
                    "allowed_root": allowed.to_string_lossy(),
                }),
                agent_group_id: Some(ag),
                title: "Save skill".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("escapes"), "got: {}", err.message);
    }

    #[test]
    fn approve_save_skill_missing_dest_is_bad_request() {
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "save_skill",
            UpsertPendingApproval {
                request_id: "r-nodest".into(),
                payload: json!({ "name": "my-skill", "content": A4_SKILL }),
                agent_group_id: Some(ag),
                title: "Save skill".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
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
    fn approve_enable_preview_flips_switch_and_creates_row_if_missing() {
        // M18 V2: the disabled-group case where NO config row exists yet. The
        // apply arm must create a defaults row then flip preview_enabled on.
        use copperclaw_db::tables::container_configs;
        let db = db();
        let ag = seed_ag(&db);
        // No container_configs row for `ag` yet.
        assert!(container_configs::get(&db, ag).unwrap().is_none());
        let id = insert_pending(
            &db,
            "enable_preview",
            UpsertPendingApproval {
                request_id: "r-preview".into(),
                payload: json!({}),
                agent_group_id: Some(ag),
                title: "Enable previews for this group?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["side_effect"]["kind"], "enable_preview");
        assert_eq!(v["side_effect"]["preview_enabled"], true);
        // The row now exists with preview_enabled = true.
        let cfg = container_configs::get(&db, ag).unwrap().unwrap();
        assert!(cfg.preview_enabled);
        // Decision log names "host" for the CLI path.
        let decs = pending_approvals::list_decisions(&db, Some(id), 10).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].outcome, DecisionOutcome::Approve);
    }

    #[test]
    fn approve_enable_preview_missing_agent_group_is_bad_request() {
        let db = db();
        let id = insert_pending(
            &db,
            "enable_preview",
            UpsertPendingApproval {
                request_id: "r-preview-noag".into(),
                payload: json!({}),
                agent_group_id: None,
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn deny_enable_preview_leaves_switch_off() {
        // Denying the card must NOT enable previews.
        use copperclaw_db::tables::container_configs;
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "enable_preview",
            UpsertPendingApproval {
                request_id: "r-preview-deny".into(),
                payload: json!({}),
                agent_group_id: Some(ag),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        deny(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        // No row was created / enabled by a deny.
        assert!(container_configs::get(&db, ag).unwrap().is_none());
    }

    #[test]
    fn approve_credentialed_external_action_grants_and_resolves_row() {
        // M18 V5: the real apply arm. Approving a credentialed_external_action
        // row (a public-tunnel exposure) must succeed, flip the row to approved,
        // log the decision, and echo the exposure specifics — NOT refuse.
        let db = db();
        let ag = seed_ag(&db);
        let id = insert_pending(
            &db,
            "credentialed_external_action",
            UpsertPendingApproval {
                request_id: "tunnel:sess:8100".into(),
                payload: json!({
                    "kind": "tunnel",
                    "provider": "cloudflared",
                    "host_port": 8100,
                    "upstream": "http://127.0.0.1:8100",
                }),
                agent_group_id: Some(ag),
                title: "Expose this preview to the public internet?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let v = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["side_effect"]["kind"], "credentialed_external_action");
        assert_eq!(v["side_effect"]["action_kind"], "tunnel");
        assert_eq!(v["side_effect"]["host_port"], 8100);
        assert_eq!(v["side_effect"]["upstream"], "http://127.0.0.1:8100");
        // Row is approved and the decision is logged.
        let after = get_row(&db, id).unwrap();
        assert_eq!(after.status, ApprovalStatus::Approved);
        let decs = pending_approvals::list_decisions(&db, Some(id), 10).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].outcome, DecisionOutcome::Approve);
    }

    #[test]
    fn deny_credentialed_external_action_leaves_no_grant() {
        // Denying must NOT grant the action; the row settles denied.
        let db = db();
        let id = insert_pending(
            &db,
            "credentialed_external_action",
            UpsertPendingApproval {
                request_id: "tunnel:sess:8101".into(),
                payload: json!({"kind": "tunnel", "host_port": 8101}),
                title: "Expose this preview to the public internet?".into(),
                options: vec![],
                ..Default::default()
            },
        );
        deny(&json!({"id": id.as_uuid().to_string()}), &db).unwrap();
        let after = get_row(&db, id).unwrap();
        assert_eq!(after.status, ApprovalStatus::Denied);
    }

    #[test]
    fn approve_one_cli_is_still_refused() {
        // The refusal arm must survive V5 splitting it out from
        // credentialed_external_action.
        let db = db();
        let id = insert_pending(
            &db,
            "one_cli",
            UpsertPendingApproval {
                request_id: "one-cli-x".into(),
                payload: json!({}),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        );
        let err = approve(&json!({"id": id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
        // Left pending (never a silent success).
        let still = get_row(&db, id).unwrap();
        assert_eq!(still.status, ApprovalStatus::Pending);
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

    // -----------------------------------------------------------------------
    // F3c: expiry stamps the card terminal
    // -----------------------------------------------------------------------

    #[test]
    fn expire_and_edit_cards_stamps_only_lapsed_cards() {
        use copperclaw_modules::context::MockDispatcher;
        let db = db();
        // Overdue, has a delivered card → swept AND stamped.
        let with_card = upsert(
            &db,
            UpsertPendingApproval {
                request_id: "exp-card".into(),
                action: "sender".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("100".into()),
                platform_message_id: Some("card-1".into()),
                expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(5)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap()
        .approval_id;
        // Overdue, but NO delivered card → swept, not stamped (nothing to edit).
        let no_card = upsert(
            &db,
            UpsertPendingApproval {
                request_id: "exp-nocard".into(),
                action: "sender".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("100".into()),
                platform_message_id: None,
                expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(5)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap()
        .approval_id;
        // Live (not overdue) → untouched.
        let live = upsert(
            &db,
            UpsertPendingApproval {
                request_id: "exp-live".into(),
                action: "sender".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("100".into()),
                platform_message_id: Some("card-live".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap()
        .approval_id;

        let mock = MockDispatcher::new();
        let dispatcher: std::sync::Arc<dyn DeliveryDispatcher> = mock.clone();
        let edited = expire_and_edit_cards(&db, &dispatcher);
        assert_eq!(
            edited, 1,
            "only the lapsed card with a message id is stamped"
        );

        // Both overdue rows are now expired; the live one stays pending.
        assert_eq!(
            get_row(&db, with_card).unwrap().status,
            ApprovalStatus::Expired
        );
        assert_eq!(
            get_row(&db, no_card).unwrap().status,
            ApprovalStatus::Expired
        );
        assert_eq!(get_row(&db, live).unwrap().status, ApprovalStatus::Pending);

        // Exactly one terminal edit, addressed by the recorded message id.
        let edits = mock.edits.lock().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].1, "card-1");
        assert_eq!(edits[0].2, EXPIRED_CARD_TEXT);

        // Idempotent: a second sweep finds nothing pending-overdue.
        assert_eq!(expire_and_edit_cards(&db, &dispatcher), 0);
    }

    #[test]
    fn expire_and_edit_cards_noop_when_nothing_overdue() {
        use copperclaw_modules::context::MockDispatcher;
        let db = db();
        upsert(
            &db,
            UpsertPendingApproval {
                request_id: "fresh".into(),
                action: "sender".into(),
                payload: json!({}),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("100".into()),
                platform_message_id: Some("card-f".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        let mock = MockDispatcher::new();
        let dispatcher: std::sync::Arc<dyn DeliveryDispatcher> = mock.clone();
        assert_eq!(expire_and_edit_cards(&db, &dispatcher), 0);
        assert_eq!(mock.edit_count(), 0);
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
