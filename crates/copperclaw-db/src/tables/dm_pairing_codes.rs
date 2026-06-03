//! CRUD for `dm_pairing_codes` (migration `018_dm_pairing_codes`).
//!
//! When an unknown sender first DMs the bot, the host mints a short
//! single-use pairing code via [`mint`] and delivers it back through the
//! ordinary outbound delivery path. The operator runs
//! `cclaw pairing approve <code>` which [`consume`]s the row and promotes
//! the sender into the central `users` table — the trust surface the
//! sender-scope gate already consults on every inbound.
//!
//! Three invariants hold the abuse surface down:
//!
//! 1. **8-char codes.** Uppercase Crockford-style alphabet (no ambiguous
//!    `0/O/1/I`), so an operator can read one over a voice call.
//! 2. **~1h TTL.** A code that is never approved lapses rather than
//!    lingering as a live grant. [`sweep_expired`] flips overdue rows to
//!    `expired`; [`get_active`] never returns one.
//! 3. **Rate-limited per channel.** [`mint`] refuses once
//!    [`MAX_ACTIVE_CODES_PER_CHANNEL`] *active, unexpired* codes already
//!    exist for the channel, returning [`MintError::RateLimited`]. A flood
//!    of unknown senders on one channel can't mint an unbounded queue.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, ChannelType, MessagingGroupId};
use rusqlite::{OptionalExtension, Row, params};

/// Number of characters in a minted pairing code.
pub const CODE_LEN: usize = 8;

/// Default time-to-live for a freshly minted pairing code: ~1 hour.
pub const PAIRING_CODE_TTL: chrono::Duration = chrono::Duration::hours(1);

/// Maximum number of *active, unexpired* codes a single channel may hold at
/// once. Minting beyond this returns [`MintError::RateLimited`].
pub const MAX_ACTIVE_CODES_PER_CHANNEL: usize = 3;

/// Alphabet used to render codes: uppercase letters + digits with the
/// visually ambiguous glyphs (`0 O 1 I`) removed so an operator can read a
/// code aloud without confusion.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Lifecycle state of a pairing code. `Active` is the only pairable state;
/// `Consumed` (approved) and `Expired` (lapsed) are terminal.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PairingStatus {
    Active,
    Consumed,
    Expired,
}

impl PairingStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PairingStatus::Active => "active",
            PairingStatus::Consumed => "consumed",
            PairingStatus::Expired => "expired",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(PairingStatus::Active),
            "consumed" => Some(PairingStatus::Consumed),
            "expired" => Some(PairingStatus::Expired),
            _ => None,
        }
    }
}

/// One persisted pairing code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    pub code: String,
    pub channel_type: ChannelType,
    pub identity: String,
    pub display_name: Option<String>,
    pub agent_group_id: Option<AgentGroupId>,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub status: PairingStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
}

impl PairingCode {
    /// True when the code's deadline is at or before `now`.
    #[must_use]
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }

    /// True when the code can still be paired: `active` and not past its
    /// deadline.
    #[must_use]
    pub fn is_pairable_at(&self, now: DateTime<Utc>) -> bool {
        self.status == PairingStatus::Active && !self.is_expired_at(now)
    }
}

/// Request to mint a new pairing code.
#[derive(Debug, Clone)]
pub struct MintPairingCode {
    pub channel_type: ChannelType,
    pub identity: String,
    pub display_name: Option<String>,
    pub agent_group_id: Option<AgentGroupId>,
    pub messaging_group_id: Option<MessagingGroupId>,
}

/// Why a [`mint`] failed.
#[derive(Debug)]
pub enum MintError {
    /// The channel already holds [`MAX_ACTIVE_CODES_PER_CHANNEL`] active
    /// codes; the caller should back off rather than mint another.
    RateLimited {
        channel_type: ChannelType,
        active: usize,
    },
    /// The DB call underneath the mint failed.
    Db(DbError),
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MintError::RateLimited {
                channel_type,
                active,
            } => write!(
                f,
                "pairing rate limit hit for channel `{}`: {active} active codes (max {MAX_ACTIVE_CODES_PER_CHANNEL})",
                channel_type.as_str(),
            ),
            MintError::Db(e) => write!(f, "pairing mint db error: {e}"),
        }
    }
}

impl std::error::Error for MintError {}

impl From<DbError> for MintError {
    fn from(e: DbError) -> Self {
        MintError::Db(e)
    }
}

/// Generate one random `CODE_LEN`-char code from [`CODE_ALPHABET`].
fn random_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..CODE_LEN)
        .map(|_| {
            let idx = rng.gen_range(0..CODE_ALPHABET.len());
            CODE_ALPHABET[idx] as char
        })
        .collect()
}

fn row_to_pairing_code(row: &Row<'_>) -> rusqlite::Result<PairingCode> {
    let channel_type_str: String = row.get("channel_type")?;
    let agent_group_id_str: Option<String> = row.get("agent_group_id")?;
    let agent_group_id = agent_group_id_str
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .map(AgentGroupId);
    let messaging_group_id_str: Option<String> = row.get("messaging_group_id")?;
    let messaging_group_id = messaging_group_id_str
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .map(MessagingGroupId);
    let status_str: String = row.get("status")?;
    let status = PairingStatus::parse(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown pairing status {status_str}").into(),
        )
    })?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let expires_at_str: String = row.get("expires_at")?;
    let expires_at = DateTime::parse_from_rfc3339(&expires_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let consumed_at_str: Option<String> = row.get("consumed_at")?;
    let consumed_at = consumed_at_str
        .as_deref()
        .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(PairingCode {
        code: row.get("code")?,
        channel_type: ChannelType::new(channel_type_str),
        identity: row.get("identity")?,
        display_name: row.get("display_name")?,
        agent_group_id,
        messaging_group_id,
        status,
        created_at,
        expires_at,
        consumed_at,
    })
}

/// Count active, unexpired codes for one channel as of `now`.
fn active_count(
    db: &CentralDb,
    channel_type: &ChannelType,
    now: DateTime<Utc>,
) -> Result<usize, DbError> {
    let conn = db.conn()?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM dm_pairing_codes
         WHERE channel_type = ?1 AND status = 'active' AND expires_at > ?2",
        params![channel_type.as_str(), now.to_rfc3339()],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n.max(0)).unwrap_or(0))
}

/// Mint a fresh pairing code for the given sender, stamping a `PAIRING_CODE_TTL`
/// deadline from `now`.
///
/// Rate limit: refuses with [`MintError::RateLimited`] once the channel
/// already holds [`MAX_ACTIVE_CODES_PER_CHANNEL`] active, unexpired codes.
/// Expired rows are swept first so a lapsed code frees a slot.
///
/// Independent of any existing code for the same sender — a sender that
/// re-contacts before approval mints a new code (subject to the rate
/// limit), so a lost code is recoverable.
pub fn mint(
    db: &CentralDb,
    req: MintPairingCode,
    now: DateTime<Utc>,
) -> Result<PairingCode, MintError> {
    // Lapse overdue rows so a stale active code doesn't wrongly count
    // against the rate limit.
    sweep_expired(db, now)?;

    let active = active_count(db, &req.channel_type, now)?;
    if active >= MAX_ACTIVE_CODES_PER_CHANNEL {
        return Err(MintError::RateLimited {
            channel_type: req.channel_type,
            active,
        });
    }

    let expires_at = now + PAIRING_CODE_TTL;
    let conn = db.conn()?;
    // Retry on the (astronomically unlikely) PK collision so a duplicate
    // random code never bubbles to the caller as a hard error.
    for _ in 0..8 {
        let code = random_code();
        let insert = conn.execute(
            "INSERT INTO dm_pairing_codes
               (code, channel_type, identity, display_name, agent_group_id,
                messaging_group_id, status, created_at, expires_at, consumed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, NULL)",
            params![
                code,
                req.channel_type.as_str(),
                req.identity,
                req.display_name,
                req.agent_group_id.map(|a| a.as_uuid().to_string()),
                req.messaging_group_id.map(|m| m.as_uuid().to_string()),
                now.to_rfc3339(),
                expires_at.to_rfc3339(),
            ],
        );
        match insert {
            Ok(_) => {
                return Ok(PairingCode {
                    code,
                    channel_type: req.channel_type,
                    identity: req.identity,
                    display_name: req.display_name,
                    agent_group_id: req.agent_group_id,
                    messaging_group_id: req.messaging_group_id,
                    status: PairingStatus::Active,
                    created_at: now,
                    expires_at,
                    consumed_at: None,
                });
            }
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // PK collision on the random code; retry with a new one.
                continue;
            }
            Err(e) => return Err(MintError::Db(DbError::from(e))),
        }
    }
    Err(MintError::Db(DbError::invariant(
        "could not mint a unique pairing code after 8 attempts",
    )))
}

/// Fetch one code by its token, regardless of status.
pub fn get(db: &CentralDb, code: &str) -> Result<Option<PairingCode>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT code, channel_type, identity, display_name, agent_group_id,
                    messaging_group_id, status, created_at, expires_at, consumed_at
             FROM dm_pairing_codes WHERE code = ?1",
            params![code],
            row_to_pairing_code,
        )
        .optional()?)
}

/// List codes, optionally filtered to one status. Newest-first.
pub fn list(db: &CentralDb, status: Option<PairingStatus>) -> Result<Vec<PairingCode>, DbError> {
    let conn = db.conn()?;
    if let Some(s) = status {
        let mut stmt = conn.prepare(
            "SELECT code, channel_type, identity, display_name, agent_group_id,
                    messaging_group_id, status, created_at, expires_at, consumed_at
             FROM dm_pairing_codes WHERE status = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![s.as_str()], row_to_pairing_code)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    } else {
        let mut stmt = conn.prepare(
            "SELECT code, channel_type, identity, display_name, agent_group_id,
                    messaging_group_id, status, created_at, expires_at, consumed_at
             FROM dm_pairing_codes ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_pairing_code)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Fetch one code only if it is pairable (active + unexpired) as of `now`.
/// Lapses overdue rows first so an expired-but-still-`active` row reads as
/// gone rather than pairable.
pub fn get_active(
    db: &CentralDb,
    code: &str,
    now: DateTime<Utc>,
) -> Result<Option<PairingCode>, DbError> {
    sweep_expired(db, now)?;
    Ok(get(db, code)?.filter(|c| c.is_pairable_at(now)))
}

/// Mark a code `consumed` (paired). Returns the updated row. Errors with
/// [`DbError::NotFound`] when the code doesn't exist, and
/// [`DbError::Invariant`] when the row is no longer active (already consumed
/// or expired) — the caller must surface that as a conflict.
pub fn consume(db: &CentralDb, code: &str, now: DateTime<Utc>) -> Result<PairingCode, DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE dm_pairing_codes SET status = 'consumed', consumed_at = ?2
         WHERE code = ?1 AND status = 'active'",
        params![code, now.to_rfc3339()],
    )?;
    drop(conn);
    if n == 0 {
        // Distinguish "no such code" from "code is not active".
        return match get(db, code)? {
            None => Err(DbError::NotFound),
            Some(_) => Err(DbError::invariant("pairing code is not active")),
        };
    }
    get(db, code)?.ok_or(DbError::NotFound)
}

/// Flip every `active` row whose `expires_at` is at or before `now` to
/// `expired`. Returns the codes that lapsed. Idempotent.
pub fn sweep_expired(db: &CentralDb, now: DateTime<Utc>) -> Result<Vec<String>, DbError> {
    let mut conn = db.conn()?;
    let now_str = now.to_rfc3339();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let codes: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT code FROM dm_pairing_codes
             WHERE status = 'active' AND expires_at <= ?1",
        )?;
        let rows = stmt.query_map(params![now_str], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if !codes.is_empty() {
        tx.execute(
            "UPDATE dm_pairing_codes SET status = 'expired'
             WHERE status = 'active' AND expires_at <= ?1",
            params![now_str],
        )?;
    }
    tx.commit()?;
    Ok(codes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn req(channel: &str, identity: &str) -> MintPairingCode {
        MintPairingCode {
            channel_type: ChannelType::new(channel),
            identity: identity.into(),
            display_name: Some(format!("{channel}/{identity}")),
            agent_group_id: None,
            messaging_group_id: None,
        }
    }

    #[test]
    fn mint_produces_eight_char_active_code() {
        let db = db();
        let now = Utc::now();
        let c = mint(&db, req("telegram", "u-1"), now).unwrap();
        assert_eq!(c.code.len(), CODE_LEN);
        assert!(c.code.chars().all(|ch| CODE_ALPHABET.contains(&(ch as u8))));
        assert_eq!(c.status, PairingStatus::Active);
        assert_eq!(c.expires_at, now + PAIRING_CODE_TTL);
        assert!(c.is_pairable_at(now));
    }

    #[test]
    fn get_round_trips() {
        let db = db();
        let c = mint(&db, req("slack", "U-7"), Utc::now()).unwrap();
        let got = get(&db, &c.code).unwrap().unwrap();
        assert_eq!(got, c);
        assert!(get(&db, "NOSUCHCD").unwrap().is_none());
    }

    #[test]
    fn rate_limit_caps_active_codes_per_channel() {
        let db = db();
        let now = Utc::now();
        for i in 0..MAX_ACTIVE_CODES_PER_CHANNEL {
            mint(&db, req("telegram", &format!("u-{i}")), now).unwrap();
        }
        let err = mint(&db, req("telegram", "u-overflow"), now).unwrap_err();
        match err {
            MintError::RateLimited { active, .. } => {
                assert_eq!(active, MAX_ACTIVE_CODES_PER_CHANNEL);
            }
            MintError::Db(e) => panic!("expected RateLimited, got Db({e})"),
        }
        // A different channel is unaffected by the telegram cap.
        assert!(mint(&db, req("slack", "U-1"), now).is_ok());
    }

    #[test]
    fn expired_codes_free_a_rate_limit_slot() {
        let db = db();
        let stale = Utc::now() - chrono::Duration::hours(2);
        // Fill the channel with codes that are already past their TTL.
        for i in 0..MAX_ACTIVE_CODES_PER_CHANNEL {
            mint(&db, req("telegram", &format!("old-{i}")), stale).unwrap();
        }
        // Minting `now` sweeps the stale codes first, freeing all slots.
        let fresh = mint(&db, req("telegram", "u-fresh"), Utc::now());
        assert!(
            fresh.is_ok(),
            "expired codes must not count against the cap"
        );
    }

    #[test]
    fn get_active_excludes_expired_and_consumed() {
        let db = db();
        let now = Utc::now();
        let c = mint(&db, req("discord", "D-1"), now).unwrap();
        // Active + unexpired -> returned.
        assert!(get_active(&db, &c.code, now).unwrap().is_some());
        // Past its TTL -> swept to expired, no longer pairable.
        let later = now + PAIRING_CODE_TTL + chrono::Duration::minutes(1);
        assert!(get_active(&db, &c.code, later).unwrap().is_none());
        let after = get(&db, &c.code).unwrap().unwrap();
        assert_eq!(after.status, PairingStatus::Expired);
    }

    #[test]
    fn consume_marks_consumed_once() {
        let db = db();
        let now = Utc::now();
        let c = mint(&db, req("telegram", "u-9"), now).unwrap();
        let consumed = consume(&db, &c.code, now).unwrap();
        assert_eq!(consumed.status, PairingStatus::Consumed);
        assert_eq!(consumed.consumed_at, Some(now));
        // Second consume is a conflict (no longer active).
        let err = consume(&db, &c.code, now).unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
        // Unknown code is NotFound.
        assert!(matches!(
            consume(&db, "ABABABAB", now).unwrap_err(),
            DbError::NotFound
        ));
    }

    #[test]
    fn consume_rejects_expired_code() {
        let db = db();
        let now = Utc::now();
        let c = mint(&db, req("telegram", "u-exp"), now).unwrap();
        let later = now + PAIRING_CODE_TTL + chrono::Duration::minutes(1);
        sweep_expired(&db, later).unwrap();
        let err = consume(&db, &c.code, later).unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
    }

    #[test]
    fn sweep_expired_is_idempotent() {
        let db = db();
        let stale = Utc::now() - chrono::Duration::hours(2);
        let c = mint(&db, req("telegram", "u-stale"), stale).unwrap();
        let now = Utc::now();
        let swept = sweep_expired(&db, now).unwrap();
        assert_eq!(swept, vec![c.code.clone()]);
        // Second sweep is a no-op.
        assert!(sweep_expired(&db, now).unwrap().is_empty());
        assert_eq!(
            get(&db, &c.code).unwrap().unwrap().status,
            PairingStatus::Expired
        );
    }

    #[test]
    fn list_filters_by_status() {
        let db = db();
        let now = Utc::now();
        let a = mint(&db, req("telegram", "u-a"), now).unwrap();
        let b = mint(&db, req("telegram", "u-b"), now).unwrap();
        consume(&db, &b.code, now).unwrap();
        let active = list(&db, Some(PairingStatus::Active)).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].code, a.code);
        let consumed = list(&db, Some(PairingStatus::Consumed)).unwrap();
        assert_eq!(consumed.len(), 1);
        assert_eq!(consumed[0].code, b.code);
        assert_eq!(list(&db, None).unwrap().len(), 2);
    }

    #[test]
    fn status_round_trips_through_strings() {
        for s in [
            PairingStatus::Active,
            PairingStatus::Consumed,
            PairingStatus::Expired,
        ] {
            assert_eq!(PairingStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(PairingStatus::parse("bogus"), None);
    }
}
