//! Schedule parsing + next-fire computation.
//!
//! This crate owns the parser and evaluator; the MCP tool handlers in
//! `copperclaw-mcp` (`schedule_task`, `list_tasks`, …) call into these
//! functions and persist the resulting `ScheduledTask` rows.
//!
//! Supported `When` syntaxes:
//!
//! * ISO-8601 UTC absolute timestamp — `"2026-05-21T15:00:00Z"`,
//!   `"2026-05-21T15:00Z"`, `"2026-05-21T15:00:00+00:00"`.
//! * Relative offsets — `"in 5m"`, `"in 30s"`, `"in 2h"`, `"in 3d"`.
//! * Daily-at — `"daily at 09:00"`, `"daily at 9:30"`.
//! * Cron — five-field crontab (`"0 */2 * * *"`). Six-field cron (with
//!   seconds) is also accepted.

use crate::context::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput, Module, ModuleContext,
};
use crate::error::ModuleError;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use copperclaw_types::{AgentGroupId, MessageKind, OutboundMessage, SessionId};
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Errors produced by the scheduling parser / evaluator.
#[derive(Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleError {
    #[error("schedule string is empty")]
    Empty,
    #[error("unsupported schedule syntax: `{0}`")]
    Unsupported(String),
    #[error("invalid relative offset `{0}`")]
    BadOffset(String),
    #[error("invalid time-of-day `{0}`")]
    BadTimeOfDay(String),
    #[error("invalid timestamp `{0}`")]
    BadTimestamp(String),
    #[error("invalid cron expression `{0}`")]
    BadCron(String),
    #[error("invalid recurrence `{0}`")]
    BadRecurrence(String),
}

/// A parsed schedule. The `parse_when` function produces one of these; the
/// host stores either the absolute instant (`At`), the cron string (`Cron`),
/// or the daily-at time (`DailyAt`) in `messages_in.process_after` /
/// `recurrence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// One-shot fire at the given absolute instant.
    At(DateTime<Utc>),
    /// Recurring at a cron expression.
    Cron(String),
    /// Recurring every day at the given UTC time-of-day.
    DailyAt { hour: u32, minute: u32 },
}

impl When {
    pub fn is_recurring(&self) -> bool {
        matches!(self, Self::Cron(_) | Self::DailyAt { .. })
    }
}

/// Parse a free-form `when` string into a [`When`].
pub fn parse_when(s: &str) -> Result<When, ScheduleError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ScheduleError::Empty);
    }

    // 1. Absolute timestamp (RFC 3339).
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(When::At(dt.with_timezone(&Utc)));
    }

    // RFC3339 without seconds, e.g. "2026-05-21T15:00Z".
    if let Some(expanded) = expand_short_rfc3339(trimmed) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(&expanded) {
            return Ok(When::At(dt.with_timezone(&Utc)));
        }
    }

    // 2. Relative offset "in N(s|m|h|d)".
    if let Some(rest) = trimmed.strip_prefix("in ") {
        return parse_in_offset(rest);
    }

    // 3. Daily at "daily at HH:MM".
    if let Some(rest) = trimmed.strip_prefix("daily at ") {
        return parse_daily_at(rest);
    }

    // 4. Cron (5 or 6 fields).
    if looks_like_cron(trimmed) {
        return parse_cron(trimmed);
    }

    Err(ScheduleError::Unsupported(trimmed.to_owned()))
}

fn expand_short_rfc3339(s: &str) -> Option<String> {
    // Match `YYYY-MM-DDTHH:MM` followed by Z or ±HH:MM.
    if s.len() < 17 {
        return None;
    }
    let bytes = s.as_bytes();
    let dash1 = bytes.get(4)? == &b'-';
    let dash2 = bytes.get(7)? == &b'-';
    let t = bytes.get(10)? == &b'T';
    let col = bytes.get(13)? == &b':';
    if !(dash1 && dash2 && t && col) {
        return None;
    }
    let tz_pos = s
        .char_indices()
        .skip(16)
        .find(|(_, c)| *c == 'Z' || *c == '+' || *c == '-')
        .map(|(i, _)| i)?;
    if tz_pos != 16 {
        return None;
    }
    Some(format!("{}:00{}", &s[..16], &s[tz_pos..]))
}

fn parse_in_offset(rest: &str) -> Result<When, ScheduleError> {
    let rest = rest.trim();
    if rest.len() < 2 {
        return Err(ScheduleError::BadOffset(rest.to_owned()));
    }
    let (num_str, unit) = rest.split_at(rest.len() - 1);
    let value: i64 = num_str
        .trim()
        .parse()
        .map_err(|_| ScheduleError::BadOffset(rest.to_owned()))?;
    if value <= 0 {
        return Err(ScheduleError::BadOffset(rest.to_owned()));
    }
    let dur = match unit {
        "s" => Duration::seconds(value),
        "m" => Duration::minutes(value),
        "h" => Duration::hours(value),
        "d" => Duration::days(value),
        _ => return Err(ScheduleError::BadOffset(rest.to_owned())),
    };
    Ok(When::At(Utc::now() + dur))
}

fn parse_daily_at(rest: &str) -> Result<When, ScheduleError> {
    let rest = rest.trim();
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 2 {
        return Err(ScheduleError::BadTimeOfDay(rest.to_owned()));
    }
    let hour: u32 = parts[0]
        .parse()
        .map_err(|_| ScheduleError::BadTimeOfDay(rest.to_owned()))?;
    let minute: u32 = parts[1]
        .parse()
        .map_err(|_| ScheduleError::BadTimeOfDay(rest.to_owned()))?;
    if hour > 23 || minute > 59 {
        return Err(ScheduleError::BadTimeOfDay(rest.to_owned()));
    }
    Ok(When::DailyAt { hour, minute })
}

fn looks_like_cron(s: &str) -> bool {
    let n = s.split_whitespace().count();
    (5..=6).contains(&n)
}

fn parse_cron(s: &str) -> Result<When, ScheduleError> {
    // Try the input verbatim first, then with seconds-optional enabled. The
    // croner builder is mutated through `&mut self`, so we have to clone.
    let mut cron = Cron::new(s);
    cron.with_seconds_optional();
    cron.parse()
        .map_err(|_| ScheduleError::BadCron(s.to_owned()))?;
    Ok(When::Cron(s.to_owned()))
}

/// Compute the next time the schedule should fire after `now`. For one-shot
/// schedules whose absolute time is in the past, returns `None`.
pub fn compute_next_fire(
    when: &When,
    now: DateTime<Utc>,
    recurrence: Option<&str>,
) -> Option<DateTime<Utc>> {
    // If a recurrence override is supplied we route through cron.
    if let Some(rec) = recurrence {
        // Empty recurrence string is treated as "no recurrence" so callers
        // can plumb an Option through a NULL DB column without re-mapping.
        if !rec.trim().is_empty() {
            return next_cron(rec, now);
        }
    }
    match when {
        When::At(t) => {
            if *t > now {
                Some(*t)
            } else {
                None
            }
        }
        When::Cron(expr) => next_cron(expr, now),
        When::DailyAt { hour, minute } => Some(next_daily(*hour, *minute, now)),
    }
}

fn next_cron(expr: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mut cron = Cron::new(expr);
    cron.with_seconds_optional();
    let cron = cron.parse().ok()?;
    cron.find_next_occurrence(&now, false).ok()
}

fn next_daily(hour: u32, minute: u32, now: DateTime<Utc>) -> DateTime<Utc> {
    let today_time = NaiveTime::from_hms_opt(hour, minute, 0).expect("validated by parser");
    let date = now.date_naive();
    let today_dt = NaiveDateTime::new(date, today_time);
    let today_utc = Utc.from_utc_datetime(&today_dt);
    if today_utc > now {
        today_utc
    } else {
        let tomorrow = next_day(date);
        let tomorrow_dt = NaiveDateTime::new(tomorrow, today_time);
        Utc.from_utc_datetime(&tomorrow_dt)
    }
}

fn next_day(d: NaiveDate) -> NaiveDate {
    d.succ_opt()
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(d.year() + 1, 1, 1).expect("year+1 valid"))
}

// ---------------------------------------------------------------------------
// Task storage trait
// ---------------------------------------------------------------------------

/// A scheduled task as seen by the module's handler and by external
/// callers. Backed by `copperclaw-db::tables::tasks` in production; an
/// `InMemoryTaskStore` is provided here for tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub name: Option<String>,
    pub prompt: String,
    pub when_spec: String,
    pub recurrence: Option<String>,
    pub next_fire: Option<DateTime<Utc>>,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lifecycle states accepted by the task store. Mirrors
/// `copperclaw-db::tables::tasks::TaskStatus` but lives here so the modules
/// crate stays decoupled from the database layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Active,
    Paused,
    Cancelled,
    Completed,
}

/// Create-task spec passed to a [`TaskStore`].
#[derive(Debug, Clone)]
pub struct CreateTaskSpec {
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub name: Option<String>,
    pub prompt: String,
    pub when_spec: String,
    pub recurrence: Option<String>,
    pub next_fire: Option<DateTime<Utc>>,
}

/// Patch-task spec passed to a [`TaskStore::update`].
#[derive(Debug, Clone, Default)]
pub struct UpdateTaskFields {
    pub prompt: Option<String>,
    pub when_spec: Option<String>,
    /// Outer Option = "do you want to change recurrence?".
    /// Inner Option = "set to None / Some".
    pub recurrence: Option<Option<String>>,
    pub next_fire: Option<Option<DateTime<Utc>>>,
}

/// Persistent backing store for scheduled tasks. The host crate provides
/// a sqlite-backed impl; tests use [`InMemoryTaskStore`].
///
/// The store is a thin CRUD interface — schedule parsing /
/// next-fire computation stays on the caller side. That way the same
/// store can be reused by the sweep loop (which queries directly via
/// SQL for due tasks).
pub trait TaskStore: Send + Sync {
    fn create(&self, spec: CreateTaskSpec) -> Result<TaskRecord, ModuleError>;
    fn get(&self, id: &str) -> Result<Option<TaskRecord>, ModuleError>;
    fn list_for_session(&self, session_id: SessionId) -> Result<Vec<TaskRecord>, ModuleError>;
    fn set_status(&self, id: &str, status: TaskStatus) -> Result<(), ModuleError>;
    fn update(&self, id: &str, fields: UpdateTaskFields) -> Result<TaskRecord, ModuleError>;
}

/// In-memory implementation of [`TaskStore`] used by the module's unit
/// tests and as a safe default before the host wires up the sqlite store.
#[derive(Default)]
pub struct InMemoryTaskStore {
    inner: Mutex<HashMap<String, TaskRecord>>,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn all(&self) -> Vec<TaskRecord> {
        self.inner.lock().unwrap().values().cloned().collect()
    }
}

impl TaskStore for InMemoryTaskStore {
    fn create(&self, spec: CreateTaskSpec) -> Result<TaskRecord, ModuleError> {
        let now = Utc::now();
        let id = format!("task_{}", uuid::Uuid::now_v7());
        let rec = TaskRecord {
            id: id.clone(),
            agent_group_id: spec.agent_group_id,
            session_id: spec.session_id,
            name: spec.name,
            prompt: spec.prompt,
            when_spec: spec.when_spec,
            recurrence: spec.recurrence,
            next_fire: spec.next_fire,
            status: TaskStatus::Active,
            created_at: now,
            updated_at: now,
        };
        self.inner.lock().unwrap().insert(id, rec.clone());
        Ok(rec)
    }

    fn get(&self, id: &str) -> Result<Option<TaskRecord>, ModuleError> {
        Ok(self.inner.lock().unwrap().get(id).cloned())
    }

    fn list_for_session(&self, session_id: SessionId) -> Result<Vec<TaskRecord>, ModuleError> {
        let mut out: Vec<TaskRecord> = self
            .inner
            .lock()
            .unwrap()
            .values()
            .filter(|t| t.session_id == session_id)
            .cloned()
            .collect();
        out.sort_by_key(|t| t.created_at);
        Ok(out)
    }

    fn set_status(&self, id: &str, status: TaskStatus) -> Result<(), ModuleError> {
        let mut guard = self.inner.lock().unwrap();
        let rec = guard
            .get_mut(id)
            .ok_or_else(|| ModuleError::other("scheduling", format!("task not found: {id}")))?;
        rec.status = status;
        rec.updated_at = Utc::now();
        Ok(())
    }

    fn update(&self, id: &str, fields: UpdateTaskFields) -> Result<TaskRecord, ModuleError> {
        let mut guard = self.inner.lock().unwrap();
        let rec = guard
            .get_mut(id)
            .ok_or_else(|| ModuleError::other("scheduling", format!("task not found: {id}")))?;
        if let Some(p) = fields.prompt {
            rec.prompt = p;
        }
        if let Some(w) = fields.when_spec {
            rec.when_spec = w;
        }
        if let Some(rec_v) = fields.recurrence {
            rec.recurrence = rec_v;
        }
        if let Some(nf) = fields.next_fire {
            rec.next_fire = nf;
        }
        rec.updated_at = Utc::now();
        Ok(rec.clone())
    }
}

/// Scheduling module. Owns a [`TaskStore`] and registers the `"schedule"`
/// delivery action so the host can persist tasks the agent creates via
/// the `schedule_task` MCP tool.
pub struct SchedulingModule {
    store: Arc<dyn TaskStore>,
}

impl Default for SchedulingModule {
    fn default() -> Self {
        Self {
            store: Arc::new(InMemoryTaskStore::new()),
        }
    }
}

impl SchedulingModule {
    /// Build a module with a caller-supplied [`TaskStore`]. The host
    /// crate uses this with the sqlite-backed store; tests use the
    /// in-memory default.
    pub fn with_store(store: Arc<dyn TaskStore>) -> Self {
        Self { store }
    }

    /// Reusable handle to the underlying [`TaskStore`]; useful for tests
    /// that want to assert the persisted rows after invoking the handler.
    pub fn store(&self) -> &Arc<dyn TaskStore> {
        &self.store
    }
}

#[async_trait]
impl Module for SchedulingModule {
    fn name(&self) -> &'static str {
        "scheduling"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        ctx.register_delivery_action(
            "schedule",
            Arc::new(ScheduleHandler {
                store: Arc::clone(&self.store),
            }),
        );
        Ok(())
    }
}

/// Delivery-action handler for the `"schedule"` system action. Drives a
/// [`TaskStore`].
pub struct ScheduleHandler {
    store: Arc<dyn TaskStore>,
}

impl ScheduleHandler {
    pub fn new(store: Arc<dyn TaskStore>) -> Self {
        Self { store }
    }

    fn target_session_and_group(
        input: &DeliveryActionInput,
    ) -> Result<(SessionId, AgentGroupId), ModuleError> {
        let session_id = input
            .session_id
            .ok_or_else(|| ModuleError::other("scheduling", "schedule: missing session_id"))?;
        let agent_group_id = input.target.agent_group_id.ok_or_else(|| {
            ModuleError::other("scheduling", "schedule: missing agent_group_id on target")
        })?;
        Ok((session_id, agent_group_id))
    }

    fn op_create(
        &self,
        input: &DeliveryActionInput,
        payload: &serde_json::Value,
    ) -> Result<DeliveryActionOutput, ModuleError> {
        let (session_id, agent_group_id) = Self::target_session_and_group(input)?;
        let prompt = payload
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("scheduling", "schedule create: missing prompt"))?
            .to_owned();
        let when_spec = payload
            .get("when")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("scheduling", "schedule create: missing when"))?
            .to_owned();
        let name = payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let recurrence = payload
            .get("recurrence")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned);
        // Parse + compute the first fire time. Failure surfaces as a
        // module error so the delivery loop marks the row failed rather
        // than silently dropping the schedule.
        let when = parse_when(&when_spec).map_err(|e| {
            ModuleError::other("scheduling", format!("schedule create: parse error: {e}"))
        })?;
        let next_fire = compute_next_fire(&when, Utc::now(), recurrence.as_deref());

        let spec = CreateTaskSpec {
            agent_group_id,
            session_id,
            name,
            prompt,
            when_spec,
            recurrence,
            next_fire,
        };
        let rec = self.store.create(spec)?;
        // Best-effort ack carried in the action's outbound message; the
        // host records the row as delivered after this returns. We
        // don't push a chat message — the runner already echoed the
        // synchronous ack to the agent via `ToolEffectAck::Task`.
        let _ = rec;
        Ok(DeliveryActionOutput::default())
    }

    fn op_list(&self, input: &DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError> {
        let (session_id, _) = Self::target_session_and_group(input)?;
        let tasks = self.store.list_for_session(session_id)?;
        // Hand the list back to the agent via a `Chat`-kind outbound
        // message that the host's delivery loop will route to the
        // session's origin channel. The runner sees this on its next
        // turn via the inbound mirror.
        let summary: Vec<serde_json::Value> = tasks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "name": t.name,
                    "prompt": t.prompt,
                    "when": t.when_spec,
                    "recurrence": t.recurrence,
                    "next_fire": t.next_fire.map(|d| d.to_rfc3339()),
                    "status": t.status,
                })
            })
            .collect();
        let message = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "tasks": summary }),
            files: vec![],
        };
        Ok(DeliveryActionOutput {
            dispatch: None,
            message: Some(message),
        })
    }

    fn op_set_status(
        &self,
        payload: &serde_json::Value,
        status: TaskStatus,
    ) -> Result<DeliveryActionOutput, ModuleError> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("scheduling", "schedule: missing id"))?;
        self.store.set_status(id, status)?;
        Ok(DeliveryActionOutput::default())
    }

    fn op_update(&self, payload: &serde_json::Value) -> Result<DeliveryActionOutput, ModuleError> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("scheduling", "schedule update: missing id"))?
            .to_owned();
        let prompt = payload
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let when_spec = payload
            .get("when")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let recurrence_field = payload.get("recurrence").map(|v| {
            v.as_str()
                .map(str::to_owned)
                .filter(|s| !s.trim().is_empty())
        });
        // Re-compute next_fire if `when` was supplied. The recurrence
        // override (if provided in this update or inherited from the
        // existing row) shapes the schedule's recurring behaviour.
        let mut next_fire_field: Option<Option<DateTime<Utc>>> = None;
        if let Some(w) = when_spec.as_deref() {
            let when = parse_when(w).map_err(|e| {
                ModuleError::other("scheduling", format!("schedule update: parse error: {e}"))
            })?;
            let effective_recurrence: Option<String> = match recurrence_field.as_ref() {
                Some(opt) => opt.clone(),
                None => self.store.get(&id)?.and_then(|t| t.recurrence),
            };
            next_fire_field = Some(compute_next_fire(
                &when,
                Utc::now(),
                effective_recurrence.as_deref(),
            ));
        }
        let fields = UpdateTaskFields {
            prompt,
            when_spec,
            recurrence: recurrence_field,
            next_fire: next_fire_field,
        };
        self.store.update(&id, fields)?;
        Ok(DeliveryActionOutput::default())
    }
}

impl DeliveryActionHandler for ScheduleHandler {
    fn handle(&self, input: DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError> {
        let payload = &input.payload;
        let op = payload
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("scheduling", "schedule: missing op"))?;
        let inner = payload
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        match op {
            "create" => self.op_create(&input, &inner),
            "list" => self.op_list(&input),
            "cancel" => self.op_set_status(&inner, TaskStatus::Cancelled),
            "pause" => self.op_set_status(&inner, TaskStatus::Paused),
            "resume" => self.op_set_status(&inner, TaskStatus::Active),
            "update" => self.op_update(&inner),
            other => Err(ModuleError::other(
                "scheduling",
                format!("schedule: unknown op `{other}`"),
            )),
        }
    }
}

impl FromStr for When {
    type Err = ScheduleError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_when(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockModuleContext;
    use chrono::Timelike;

    #[test]
    fn parse_rfc3339_full() {
        let w = parse_when("2026-05-21T15:00:00Z").unwrap();
        assert!(matches!(w, When::At(_)));
    }

    #[test]
    fn parse_rfc3339_short_no_seconds() {
        let w = parse_when("2026-05-21T15:00Z").unwrap();
        if let When::At(t) = w {
            assert_eq!(t.hour(), 15);
            assert_eq!(t.minute(), 0);
        } else {
            panic!("expected At");
        }
    }

    #[test]
    fn parse_rfc3339_with_offset() {
        let w = parse_when("2026-05-21T15:00:00+02:00").unwrap();
        if let When::At(t) = w {
            // 15:00 +02:00 is 13:00 UTC.
            assert_eq!(t.hour(), 13);
        } else {
            panic!();
        }
    }

    #[test]
    fn parse_in_seconds_minutes_hours_days() {
        assert!(matches!(parse_when("in 30s").unwrap(), When::At(_)));
        assert!(matches!(parse_when("in 5m").unwrap(), When::At(_)));
        assert!(matches!(parse_when("in 2h").unwrap(), When::At(_)));
        assert!(matches!(parse_when("in 1d").unwrap(), When::At(_)));
    }

    #[test]
    fn parse_in_offset_is_in_future() {
        let before = Utc::now();
        if let When::At(t) = parse_when("in 5m").unwrap() {
            assert!(t > before);
            assert!(t < before + Duration::minutes(6));
        } else {
            panic!();
        }
    }

    #[test]
    fn parse_in_rejects_zero_and_negative() {
        assert!(parse_when("in 0m").is_err());
        assert!(parse_when("in -1m").is_err());
    }

    #[test]
    fn parse_in_rejects_unknown_unit() {
        assert!(matches!(
            parse_when("in 5x").unwrap_err(),
            ScheduleError::BadOffset(_)
        ));
    }

    #[test]
    fn parse_in_rejects_empty_number() {
        assert!(parse_when("in m").is_err());
    }

    #[test]
    fn parse_daily_at_works() {
        let w = parse_when("daily at 09:30").unwrap();
        assert_eq!(
            w,
            When::DailyAt {
                hour: 9,
                minute: 30
            }
        );
    }

    #[test]
    fn parse_daily_at_single_digit_hour() {
        let w = parse_when("daily at 9:00").unwrap();
        assert_eq!(w, When::DailyAt { hour: 9, minute: 0 });
    }

    #[test]
    fn parse_daily_at_rejects_oob_hour() {
        assert!(parse_when("daily at 25:00").is_err());
    }

    #[test]
    fn parse_daily_at_rejects_oob_minute() {
        assert!(parse_when("daily at 09:60").is_err());
    }

    #[test]
    fn parse_daily_at_rejects_malformed() {
        assert!(parse_when("daily at noon").is_err());
        assert!(parse_when("daily at 9").is_err());
    }

    #[test]
    fn parse_cron_5_field() {
        let w = parse_when("0 */2 * * *").unwrap();
        assert_eq!(w, When::Cron("0 */2 * * *".into()));
    }

    #[test]
    fn parse_cron_6_field() {
        let w = parse_when("0 0 */2 * * *").unwrap();
        assert!(matches!(w, When::Cron(_)));
    }

    #[test]
    fn parse_cron_rejects_garbage() {
        // Three words: doesn't look like cron (5-6 fields), so unsupported.
        assert!(matches!(
            parse_when("hello there friend").unwrap_err(),
            ScheduleError::Unsupported(_)
        ));
        // Looks-like-cron, but croner rejects it.
        assert!(matches!(
            parse_when("99 99 99 99 99").unwrap_err(),
            ScheduleError::BadCron(_)
        ));
    }

    #[test]
    fn parse_empty_returns_empty() {
        assert_eq!(parse_when("").unwrap_err(), ScheduleError::Empty);
        assert_eq!(parse_when("   ").unwrap_err(), ScheduleError::Empty);
    }

    #[test]
    fn parse_unsupported_returns_unsupported() {
        let err = parse_when("at-some-point-soon").unwrap_err();
        assert!(matches!(err, ScheduleError::Unsupported(_)));
    }

    #[test]
    fn from_str_uses_parse_when() {
        let w: When = "in 5m".parse().unwrap();
        assert!(matches!(w, When::At(_)));
    }

    #[test]
    fn compute_next_for_at_future() {
        let now = Utc::now();
        let later = now + Duration::minutes(5);
        let w = When::At(later);
        assert_eq!(compute_next_fire(&w, now, None), Some(later));
    }

    #[test]
    fn compute_next_for_at_past_returns_none() {
        let now = Utc::now();
        let past = now - Duration::minutes(5);
        let w = When::At(past);
        assert!(compute_next_fire(&w, now, None).is_none());
    }

    #[test]
    fn compute_next_daily_today_if_future() {
        let now = Utc.with_ymd_and_hms(2026, 5, 21, 8, 0, 0).unwrap();
        let w = When::DailyAt {
            hour: 9,
            minute: 30,
        };
        let next = compute_next_fire(&w, now, None).unwrap();
        assert_eq!(next.year(), 2026);
        assert_eq!(next.month(), 5);
        assert_eq!(next.day(), 21);
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn compute_next_daily_tomorrow_if_past() {
        let now = Utc.with_ymd_and_hms(2026, 5, 21, 10, 0, 0).unwrap();
        let w = When::DailyAt {
            hour: 9,
            minute: 30,
        };
        let next = compute_next_fire(&w, now, None).unwrap();
        assert_eq!(next.day(), 22);
        assert_eq!(next.hour(), 9);
    }

    #[test]
    fn compute_next_daily_year_rollover() {
        let now = Utc.with_ymd_and_hms(2026, 12, 31, 23, 0, 0).unwrap();
        let w = When::DailyAt { hour: 1, minute: 0 };
        let next = compute_next_fire(&w, now, None).unwrap();
        assert_eq!(next.year(), 2027);
        assert_eq!(next.month(), 1);
        assert_eq!(next.day(), 1);
    }

    #[test]
    fn compute_next_cron_basic() {
        let now = Utc.with_ymd_and_hms(2026, 5, 21, 10, 30, 0).unwrap();
        let w = When::Cron("0 */2 * * *".into());
        let next = compute_next_fire(&w, now, None).unwrap();
        // Next "minute=0, every 2nd hour" after 10:30 is 12:00.
        assert_eq!(next.hour(), 12);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn compute_next_cron_invalid_returns_none() {
        let w = When::Cron("not cron".into());
        assert!(compute_next_fire(&w, Utc::now(), None).is_none());
    }

    #[test]
    fn compute_next_recurrence_overrides_when() {
        let now = Utc.with_ymd_and_hms(2026, 5, 21, 10, 30, 0).unwrap();
        let w = When::At(now + Duration::days(10));
        let next = compute_next_fire(&w, now, Some("0 */2 * * *")).unwrap();
        assert_eq!(next.hour(), 12);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn empty_recurrence_is_ignored() {
        let now = Utc.with_ymd_and_hms(2026, 5, 21, 10, 30, 0).unwrap();
        let later = now + Duration::hours(1);
        let w = When::At(later);
        assert_eq!(compute_next_fire(&w, now, Some("")), Some(later));
    }

    #[test]
    fn when_is_recurring() {
        assert!(!When::At(Utc::now()).is_recurring());
        assert!(When::Cron("* * * * *".into()).is_recurring());
        assert!(When::DailyAt { hour: 1, minute: 0 }.is_recurring());
    }

    #[test]
    fn when_serde_roundtrip() {
        let at = When::At(Utc.with_ymd_and_hms(2026, 5, 21, 10, 30, 0).unwrap());
        let cron = When::Cron("0 0 * * *".into());
        let daily = When::DailyAt { hour: 9, minute: 0 };
        for w in [at, cron, daily] {
            let s = serde_json::to_string(&w).unwrap();
            let back: When = serde_json::from_str(&s).unwrap();
            assert_eq!(w, back);
        }
    }

    #[test]
    fn schedule_error_serde_roundtrip() {
        for e in [
            ScheduleError::Empty,
            ScheduleError::Unsupported("x".into()),
            ScheduleError::BadOffset("x".into()),
            ScheduleError::BadTimeOfDay("x".into()),
            ScheduleError::BadTimestamp("x".into()),
            ScheduleError::BadCron("x".into()),
            ScheduleError::BadRecurrence("x".into()),
        ] {
            let s = serde_json::to_string(&e).unwrap();
            let back: ScheduleError = serde_json::from_str(&s).unwrap();
            assert_eq!(e, back);
        }
    }

    #[tokio::test]
    async fn install_registers_schedule_action() {
        let m = SchedulingModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        assert_eq!(ctx.delivery_actions(), vec!["schedule".to_string()]);
        assert_eq!(m.name(), "scheduling");
    }

    // -----------------------------------------------------------------------
    // ScheduleHandler tests
    // -----------------------------------------------------------------------

    use crate::context::DispatchTarget;

    fn handler_with_store() -> (ScheduleHandler, Arc<InMemoryTaskStore>) {
        let store = Arc::new(InMemoryTaskStore::new());
        let handler = ScheduleHandler::new(store.clone() as Arc<dyn TaskStore>);
        (handler, store)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dispatch_input(
        op: &str,
        payload: serde_json::Value,
        agent_group_id: AgentGroupId,
        session_id: SessionId,
    ) -> DeliveryActionInput {
        DeliveryActionInput {
            action: "schedule".into(),
            payload: serde_json::json!({ "op": op, "payload": payload }),
            target: DispatchTarget {
                channel_type: None,
                platform_id: None,
                thread_id: None,
                agent_group_id: Some(agent_group_id),
            },
            session_id: Some(session_id),
            row_id: None,
        }
    }

    #[test]
    fn schedule_create_persists_task() {
        let (handler, store) = handler_with_store();
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let input = dispatch_input(
            "create",
            serde_json::json!({
                "name": "morning ping",
                "prompt": "remind me",
                "when": "daily at 09:00",
                "recurrence": null,
            }),
            ag,
            sess,
        );
        let out = handler.handle(input).unwrap();
        assert!(out.message.is_none());
        assert_eq!(store.all().len(), 1);
        let t = &store.all()[0];
        assert_eq!(t.prompt, "remind me");
        assert_eq!(t.session_id, sess);
        assert_eq!(t.agent_group_id, ag);
        assert_eq!(t.status, TaskStatus::Active);
        assert!(t.next_fire.is_some());
    }

    #[test]
    fn schedule_create_returns_id_via_store() {
        // The handler doesn't surface the id back to the runner (the
        // synchronous ack already carries it). We assert the persisted
        // row exposes a stable id.
        let (handler, store) = handler_with_store();
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        handler
            .handle(dispatch_input(
                "create",
                serde_json::json!({
                    "prompt": "a",
                    "when": "in 1m",
                }),
                ag,
                sess,
            ))
            .unwrap();
        let id = store.all()[0].id.clone();
        assert!(id.starts_with("task_"));
        // Round-trip: looking up by id returns the row.
        assert!(store.get(&id).unwrap().is_some());
    }

    #[test]
    fn schedule_cancel_marks_status() {
        let (handler, store) = handler_with_store();
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        handler
            .handle(dispatch_input(
                "create",
                serde_json::json!({"prompt": "a", "when": "in 1m"}),
                ag,
                sess,
            ))
            .unwrap();
        let id = store.all()[0].id.clone();
        handler
            .handle(dispatch_input(
                "cancel",
                serde_json::json!({"id": id}),
                ag,
                sess,
            ))
            .unwrap();
        assert_eq!(
            store.get(&id).unwrap().unwrap().status,
            TaskStatus::Cancelled
        );
    }

    #[test]
    fn schedule_pause_resume() {
        let (handler, store) = handler_with_store();
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        handler
            .handle(dispatch_input(
                "create",
                serde_json::json!({"prompt": "a", "when": "in 1m"}),
                ag,
                sess,
            ))
            .unwrap();
        let id = store.all()[0].id.clone();
        handler
            .handle(dispatch_input(
                "pause",
                serde_json::json!({"id": id}),
                ag,
                sess,
            ))
            .unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().status, TaskStatus::Paused);
        handler
            .handle(dispatch_input(
                "resume",
                serde_json::json!({"id": id}),
                ag,
                sess,
            ))
            .unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().status, TaskStatus::Active);
    }

    #[test]
    fn schedule_update_patches_when() {
        let (handler, store) = handler_with_store();
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        handler
            .handle(dispatch_input(
                "create",
                serde_json::json!({"prompt": "a", "when": "in 1m"}),
                ag,
                sess,
            ))
            .unwrap();
        let id = store.all()[0].id.clone();
        handler
            .handle(dispatch_input(
                "update",
                serde_json::json!({
                    "id": id,
                    "prompt": "updated",
                    "when": "daily at 09:00",
                }),
                ag,
                sess,
            ))
            .unwrap();
        let t = store.get(&id).unwrap().unwrap();
        assert_eq!(t.prompt, "updated");
        assert_eq!(t.when_spec, "daily at 09:00");
        // next_fire should be re-computed.
        assert!(t.next_fire.is_some());
    }

    #[test]
    fn schedule_list_returns_tasks_for_session() {
        let (handler, _store) = handler_with_store();
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        for i in 0..3 {
            handler
                .handle(dispatch_input(
                    "create",
                    serde_json::json!({
                        "prompt": format!("p{i}"),
                        "when": "in 1h",
                    }),
                    ag,
                    sess,
                ))
                .unwrap();
        }
        let list_out = handler
            .handle(dispatch_input("list", serde_json::json!({}), ag, sess))
            .unwrap();
        let msg = list_out.message.unwrap();
        let tasks = msg.content.get("tasks").and_then(|v| v.as_array()).unwrap();
        assert_eq!(tasks.len(), 3);
    }

    #[test]
    fn schedule_create_rejects_missing_prompt() {
        let (handler, _) = handler_with_store();
        let err = handler
            .handle(dispatch_input(
                "create",
                serde_json::json!({"when": "in 1m"}),
                AgentGroupId::new(),
                SessionId::new(),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("prompt"));
    }

    #[test]
    fn schedule_create_rejects_bad_when() {
        let (handler, _) = handler_with_store();
        let err = handler
            .handle(dispatch_input(
                "create",
                serde_json::json!({"prompt": "x", "when": "tomorrow"}),
                AgentGroupId::new(),
                SessionId::new(),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("parse error"));
    }

    #[test]
    fn schedule_unknown_op_errors() {
        let (handler, _) = handler_with_store();
        let err = handler
            .handle(dispatch_input(
                "explode",
                serde_json::json!({}),
                AgentGroupId::new(),
                SessionId::new(),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("unknown op"));
    }

    #[test]
    fn schedule_missing_session_id_errors() {
        let (handler, _) = handler_with_store();
        let mut input = dispatch_input(
            "create",
            serde_json::json!({"prompt": "x", "when": "in 1m"}),
            AgentGroupId::new(),
            SessionId::new(),
        );
        input.session_id = None;
        let err = handler.handle(input).unwrap_err();
        assert!(err.to_string().contains("session_id"));
    }
}
