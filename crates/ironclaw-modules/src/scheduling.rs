//! Schedule parsing + next-fire computation.
//!
//! This crate owns the parser and evaluator; the MCP tool handlers in
//! `ironclaw-mcp` (`schedule_task`, `list_tasks`, …) call into these
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

use crate::context::{Module, ModuleContext};
use crate::error::ModuleError;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
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

/// Scheduling module. Pure engine — no hooks registered.
pub struct SchedulingModule;

impl Default for SchedulingModule {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Module for SchedulingModule {
    fn name(&self) -> &'static str {
        "scheduling"
    }

    async fn install(&self, _ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        // No-op: the module exposes a pure-function API used by the MCP
        // schedule_* tool handlers and the host's sweep loop. It does not
        // register hooks against the module context.
        Ok(())
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
        assert_eq!(w, When::DailyAt { hour: 9, minute: 30 });
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
        let w = When::DailyAt { hour: 9, minute: 30 };
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
        let w = When::DailyAt { hour: 9, minute: 30 };
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
    async fn install_is_noop() {
        let m = SchedulingModule;
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        assert!(ctx.registered().is_empty());
        assert_eq!(m.name(), "scheduling");
    }
}
