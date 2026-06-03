//! Handlers for `pairing.*` commands.
//!
//! DM pairing codes are minted by the host's `build_pairing_notifier` when an
//! unknown sender first DMs the bot, and delivered back to that sender. The
//! operator reads the code (relayed by the user) and runs:
//!
//! - `cclaw pairing list` — show pending (active, unexpired) codes.
//! - `cclaw pairing approve <code>` — consume the code and promote the
//!   sender into the central `users` table. The sender-scope gate consults
//!   `users` on every inbound, so approval takes effect on the next message
//!   without a host restart — exactly like `cclaw approvals approve`.

use super::{db_err, req_str};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::dm_pairing_codes::{self, PairingStatus};
use copperclaw_db::tables::users;
use serde_json::{Value, json};

/// List pairing codes. Active (unexpired) codes only by default; pass
/// `{"all": true}` to include consumed / expired rows.
pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    // Lapse overdue rows first so the live list never shows a code that has
    // already passed its TTL as "active".
    dm_pairing_codes::sweep_expired(central, chrono::Utc::now()).map_err(db_err)?;
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    let filter = if all {
        None
    } else {
        Some(PairingStatus::Active)
    };
    let rows = dm_pairing_codes::list(central, filter).map_err(db_err)?;
    Ok(json!(rows.iter().map(code_to_json).collect::<Vec<_>>()))
}

/// Approve (consume) a pairing code: promote the bound sender into the
/// central `users` table and mark the code consumed. Conflicts when the code
/// is already consumed or expired; `not_found` when it doesn't exist.
pub fn approve(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let code = req_str(args, "code")?;
    let now = chrono::Utc::now();
    // Resolve the code to its bound sender first (active + unexpired only).
    let Some(row) = dm_pairing_codes::get_active(central, &code, now).map_err(db_err)? else {
        // Distinguish "no such code" from "code not actionable".
        return match dm_pairing_codes::get(central, &code).map_err(db_err)? {
            None => Err(ErrorPayload::new("not_found", "no such pairing code")),
            Some(_) => Err(ErrorPayload::new(
                "conflict",
                "pairing code is expired or already consumed",
            )),
        };
    };

    // Promote the sender into `users` — the trust surface the gate reads.
    let user = users::upsert(
        central,
        users::UpsertUser {
            kind: row.channel_type.as_str().to_owned(),
            identity: row.identity.clone(),
            display_name: row.display_name.clone(),
        },
    )
    .map_err(db_err)?;

    // Consume the code (terminal). If it lapsed between the read and here,
    // surface a conflict rather than leaving a dangling approved user.
    dm_pairing_codes::consume(central, &code, now).map_err(|e| match e {
        copperclaw_db::DbError::Invariant(_) => {
            ErrorPayload::new("conflict", "pairing code is no longer active")
        }
        other => db_err(other),
    })?;

    Ok(json!({
        "code": row.code,
        "channel_type": row.channel_type.as_str(),
        "identity": row.identity,
        "display_name": row.display_name,
        "user_id": user.id.as_uuid().to_string(),
        "paired": true,
    }))
}

fn code_to_json(c: &dm_pairing_codes::PairingCode) -> Value {
    json!({
        "code": c.code,
        "channel_type": c.channel_type.as_str(),
        "identity": c.identity,
        "display_name": c.display_name,
        "agent_group_id": c.agent_group_id.map(|a| a.as_uuid().to_string()),
        "messaging_group_id": c.messaging_group_id.map(|m| m.as_uuid().to_string()),
        "status": c.status.as_str(),
        "created_at": c.created_at.to_rfc3339(),
        "expires_at": c.expires_at.to_rfc3339(),
        "consumed_at": c.consumed_at.map(|t| t.to_rfc3339()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::dm_pairing_codes::{MintPairingCode, mint};
    use copperclaw_types::ChannelType;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn mint_one(db: &CentralDb, channel: &str, identity: &str) -> String {
        mint(
            db,
            MintPairingCode {
                channel_type: ChannelType::new(channel),
                identity: identity.into(),
                display_name: Some("Alice".into()),
                agent_group_id: None,
                messaging_group_id: None,
            },
            chrono::Utc::now(),
        )
        .unwrap()
        .code
    }

    #[test]
    fn list_empty() {
        let db = db();
        assert!(
            list(&Value::Null, &db)
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn list_shows_active_codes() {
        let db = db();
        mint_one(&db, "telegram", "u-1");
        let v = list(&Value::Null, &db).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["status"], "active");
        assert_eq!(arr[0]["identity"], "u-1");
        assert_eq!(arr[0]["channel_type"], "telegram");
        assert_eq!(arr[0]["code"].as_str().unwrap().len(), 8);
    }

    #[test]
    fn approve_promotes_sender_and_consumes_code() {
        let db = db();
        let code = mint_one(&db, "telegram", "u-42");
        let v = approve(&json!({"code": code}), &db).unwrap();
        assert_eq!(v["paired"], true);
        assert_eq!(v["identity"], "u-42");
        // The sender is now a known user.
        let user = users::get_by_identity(&db, "telegram", "u-42")
            .unwrap()
            .unwrap();
        assert_eq!(user.display_name.as_deref(), Some("Alice"));
        // The code is consumed (no longer active).
        let after = dm_pairing_codes::get(&db, &code).unwrap().unwrap();
        assert_eq!(after.status, PairingStatus::Consumed);
    }

    #[test]
    fn approve_unknown_code_is_not_found() {
        let db = db();
        let err = approve(&json!({"code": "ZZZZZZZZ"}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn approve_consumed_code_is_conflict() {
        let db = db();
        let code = mint_one(&db, "telegram", "u-9");
        approve(&json!({"code": code.clone()}), &db).unwrap();
        let err = approve(&json!({"code": code}), &db).unwrap_err();
        assert_eq!(err.code, "conflict");
    }

    #[test]
    fn approve_expired_code_is_conflict() {
        let db = db();
        // Mint with a clock far in the past so it's already lapsed now.
        let code = mint(
            &db,
            MintPairingCode {
                channel_type: ChannelType::new("slack"),
                identity: "U-exp".into(),
                display_name: None,
                agent_group_id: None,
                messaging_group_id: None,
            },
            chrono::Utc::now() - chrono::Duration::hours(2),
        )
        .unwrap()
        .code;
        let err = approve(&json!({"code": code}), &db).unwrap_err();
        assert_eq!(err.code, "conflict");
        // No user was created for an expired code.
        assert!(
            users::get_by_identity(&db, "slack", "U-exp")
                .unwrap()
                .is_none()
        );
    }
}
