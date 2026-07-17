//! Question-expiry surfacing (M21 F2 — "expire `ask_user_question` out
//! loud").
//!
//! [`copperclaw_modules::InteractiveModule::sweep_expired`] existed since
//! the module landed but no sweep loop ever called it: an unanswered
//! question (default TTL 24h) simply evaporated. The card sat in chat
//! with live option buttons, the user was never told the answer window
//! lapsed, and the asking agent's question was never resolved. This check
//! wires the module's expiry into the sweep pass and mirrors the polished
//! approval-expiry pattern (`copperclaw-host/src/handlers/approvals.rs::
//! expire_and_edit_cards`): each lapsed question is stamped terminal and
//! surfaced exactly once.
//!
//! Per expired question the check:
//!
//! 1. **Answered-later gate.** If any chat inbound arrived on the session
//!    AFTER the question was asked, the user de-facto replied (option tap
//!    or free text — both land as ordinary chat inbounds the runner
//!    already processed), so the lapse is resolved silently: no note, no
//!    synthetic result. Nothing in production calls
//!    `InteractiveModule::answer` — reply handling is the runner's job —
//!    so this observable gate is what keeps answered questions untouched.
//! 2. **One terminal user-facing note.** A `System` `edit` row targeting
//!    the original card's seq is written to the session's `outbound.db`.
//!    On edit-capable channels the delivery service's typed-edit path
//!    replaces the card in place with [`EXPIRED_QUESTION_TEXT`] — card
//!    stamped terminal, no live buttons left behind. On channels without
//!    an edit API the existing fallback emits the same text as a fresh
//!    `"(edit) …"` chat line. Either way the user sees exactly one note.
//! 3. **Unblock the asking agent.** A `System` inbound row carrying an
//!    `ask_user_question_result` payload with `status = "expired"` is
//!    written with `trigger = false`: it never spawns a container on its
//!    own, but the runner's next turn (whenever the user next writes)
//!    renders it as a `[system]` line, so the agent sees the synthetic
//!    no-answer result instead of waiting forever.
//!
//! The module's `sweep_expired` removes lapsed questions from the pending
//! set, so the check is naturally idempotent — a question is surfaced at
//! most once. Per-question DB failures are logged and skipped (the
//! question is already terminal in module state; surfacing is best
//! effort, exactly like the approval-card stamping).

use crate::error::SweepError;
use crate::service::SessionRoot;
use chrono::{DateTime, Utc};
use copperclaw_db::tables::messages_in::{WriteInbound, insert as insert_in};
use copperclaw_db::tables::messages_out::{WriteOutbound, insert as insert_out};
use copperclaw_modules::{InteractiveModule, PendingQuestion};
use copperclaw_types::{MessageId, MessageKind, SessionId};
use rusqlite::{OptionalExtension, params};

/// Terminal text stamped onto (or emitted for) a question card whose TTL
/// lapsed with no answer. Matches the card copy style of the approval
/// expiry (`EXPIRED_CARD_TEXT`): short, honest, and it tells the user the
/// one useful next step.
pub const EXPIRED_QUESTION_TEXT: &str = "This question expired before anyone answered \u{2014} just reply and I'll pick it up from there.";

/// Outcome of surfacing one expired question. Collected into
/// [`crate::service::SweepReport::questions_expired`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionExpiryEmit {
    /// The module-level question id (the runner's `q_<uuid>` string).
    pub question_id: String,
    /// Session the question originated from, when known.
    pub session_id: Option<SessionId>,
    /// True when the terminal note (the card-stamping edit row) was
    /// written to the session's outbound DB.
    pub note_emitted: bool,
    /// True when the lapse was resolved silently because a chat inbound
    /// arrived after the ask (the user de-facto answered).
    pub resolved_by_reply: bool,
}

/// Run the question-expiry check once. Selects every lapsed unanswered
/// question from the shared [`InteractiveModule`] state (TTL enforcement
/// lives in the module) and surfaces each one per the module docs above.
pub fn check(
    root: &dyn SessionRoot,
    store: &InteractiveModule,
    now: DateTime<Utc>,
) -> Vec<QuestionExpiryEmit> {
    let expired = store.sweep_expired(now);
    let mut emits = Vec::with_capacity(expired.len());
    for q in expired {
        match surface_one(root, &q, now) {
            Ok(emit) => {
                tracing::info!(
                    target: "copperclaw_host_sweep",
                    question = %emit.question_id,
                    session = ?emit.session_id.map(|s| s.as_uuid().to_string()),
                    note_emitted = emit.note_emitted,
                    resolved_by_reply = emit.resolved_by_reply,
                    "ask_user_question expired",
                );
                emits.push(emit);
            }
            Err(err) => {
                // The question is already out of the pending set; losing
                // the surfacing write is a logged degradation, not a
                // pass-aborting error (mirrors the approval-card stamp).
                tracing::warn!(
                    target: "copperclaw_host_sweep",
                    question = %q.id.0,
                    error = %err,
                    "could not surface expired question",
                );
                emits.push(QuestionExpiryEmit {
                    question_id: q.id.0.clone(),
                    session_id: q.origin.session_id,
                    note_emitted: false,
                    resolved_by_reply: false,
                });
            }
        }
    }
    emits
}

fn surface_one(
    root: &dyn SessionRoot,
    q: &PendingQuestion,
    now: DateTime<Utc>,
) -> Result<QuestionExpiryEmit, SweepError> {
    let (Some(agent_group_id), Some(session_id)) = (q.origin.agent_group_id, q.origin.session_id)
    else {
        // No provenance (a question registered outside the delivery
        // pipeline). Nothing to write anywhere — record the lapse only.
        tracing::debug!(
            target: "copperclaw_host_sweep",
            question = %q.id.0,
            "expired question has no session origin; nothing to surface",
        );
        return Ok(QuestionExpiryEmit {
            question_id: q.id.0.clone(),
            session_id: None,
            note_emitted: false,
            resolved_by_reply: false,
        });
    };

    let inbound_pool = root.inbound_pool(&agent_group_id, &session_id)?;

    // Answered-later gate: any chat inbound newer than the ask means the
    // user replied within the window and the runner already handled it.
    if chat_inbound_arrived_after(inbound_pool.conn(), q.asked_at)? {
        return Ok(QuestionExpiryEmit {
            question_id: q.id.0.clone(),
            session_id: Some(session_id),
            note_emitted: false,
            resolved_by_reply: true,
        });
    }

    let outbound_pool = root.outbound_pool(&agent_group_id, &session_id)?;

    // Resolve the original ask row's seq so the edit can stamp the
    // delivered card in place. A missing seq is fine: the delivery
    // service's edit path falls back to a fresh "(edit) …" chat line,
    // which still carries the note.
    let ask_seq = match q.origin.message_out_id {
        Some(id) => seq_for_message_id(outbound_pool.conn(), id)?,
        None => None,
    };

    let mut edit_payload = serde_json::Map::new();
    if let Some(seq) = ask_seq {
        edit_payload.insert("seq".into(), serde_json::json!(seq));
    }
    edit_payload.insert("text".into(), serde_json::json!(EXPIRED_QUESTION_TEXT));

    // Only emit the note when the question has channel routing to carry
    // it; a routeless row would spin in the delivery loop as NoRoute.
    let note_emitted = if q.origin.channel_type.is_some() && q.origin.platform_id.is_some() {
        let note = WriteOutbound {
            id: MessageId::new(),
            in_reply_to: None,
            timestamp: now,
            deliver_after: None,
            recurrence: None,
            kind: MessageKind::System,
            channel_type: q.origin.channel_type.clone(),
            platform_id: q.origin.platform_id.clone(),
            thread_id: q.origin.thread_id.clone(),
            content: serde_json::json!({ "edit": edit_payload }),
        };
        insert_out(outbound_pool.conn(), &note)?;
        true
    } else {
        tracing::debug!(
            target: "copperclaw_host_sweep",
            question = %q.id.0,
            "expired question has no channel routing; skipping user note",
        );
        false
    };

    // Synthetic no-answer result for the asking agent. `trigger = false`
    // so it never spawns a container by itself; the next turn's prompt
    // (whenever the user writes again) includes it as a `[system]` line.
    let result = WriteInbound {
        id: MessageId::new(),
        kind: MessageKind::System,
        timestamp: now,
        content: serde_json::json!({
            "ask_user_question_result": {
                "id": q.id.0,
                "status": "expired",
                "title": q.title,
                "detail": "No answer arrived before this question expired; the card was closed. If the user replies later, treat that reply as the answer.",
            }
        }),
        trigger: false,
        on_wake: false,
        process_after: None,
        recurrence: None,
        series_id: None,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        source_session_id: None,
        reply_to: None,
        is_group: None,
    };
    insert_in(inbound_pool.conn(), &result)?;

    Ok(QuestionExpiryEmit {
        question_id: q.id.0.clone(),
        session_id: Some(session_id),
        note_emitted,
        resolved_by_reply: false,
    })
}

/// True when the newest chat inbound on the session is younger than
/// `asked_at`. Rows are compared in Rust (rfc3339 parse) rather than by
/// SQL string ordering so fractional-second formatting differences can't
/// skew the gate; the newest row by `seq` is sufficient because seq is
/// insertion-ordered.
fn chat_inbound_arrived_after(
    conn: &rusqlite::Connection,
    asked_at: DateTime<Utc>,
) -> Result<bool, SweepError> {
    let latest: Option<String> = conn
        .query_row(
            "SELECT timestamp FROM messages_in
             WHERE kind = 'chat'
             ORDER BY seq DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(ts_str) = latest else {
        return Ok(false);
    };
    match DateTime::parse_from_rfc3339(&ts_str) {
        Ok(ts) => Ok(ts.with_timezone(&Utc) > asked_at),
        Err(err) => {
            tracing::warn!(
                target: "copperclaw_host_sweep",
                error = %err,
                "unparseable messages_in timestamp; treating as no reply",
            );
            Ok(false)
        }
    }
}

/// Resolve a `messages_out` row's `seq` by its id, if the row exists.
fn seq_for_message_id(
    conn: &rusqlite::Connection,
    id: MessageId,
) -> Result<Option<i64>, SweepError> {
    let seq: Option<i64> = conn
        .query_row(
            "SELECT seq FROM messages_out WHERE id = ?1",
            params![id.as_uuid().to_string()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemSessionRoot, seed_running_session};
    use chrono::{Duration as ChDuration, TimeZone};
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::tables::{messages_in, messages_out};
    use copperclaw_modules::{QuestionId, QuestionOrigin};
    use copperclaw_types::{ChannelType, Session};

    fn fixture() -> (CentralDb, MemSessionRoot, Session, DateTime<Utc>) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 7, 17, 12, 0, 0).unwrap();
        (central, root, session, now)
    }

    /// Insert the triggering chat inbound + the runner's `System` ask
    /// row, then register the pending question the way the delivery
    /// service's `AskHandler` does. Returns the question id + ask seq.
    fn seed_asked_question(
        root: &MemSessionRoot,
        session: &Session,
        store: &InteractiveModule,
        asked_at: DateTime<Utc>,
    ) -> (QuestionId, i64) {
        let inbound = root
            .inbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        messages_in::insert(
            inbound.conn(),
            &messages_in::WriteInbound {
                id: MessageId::new(),
                kind: MessageKind::Chat,
                timestamp: asked_at - ChDuration::seconds(5),
                content: serde_json::json!({"text": "tabs or spaces?"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();

        let ask_row_id = MessageId::new();
        let outbound = root
            .outbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        let ask_seq = messages_out::insert(
            outbound.conn(),
            &messages_out::WriteOutbound {
                id: ask_row_id,
                in_reply_to: None,
                timestamp: asked_at,
                deliver_after: None,
                recurrence: None,
                kind: MessageKind::System,
                channel_type: None,
                platform_id: None,
                thread_id: None,
                content: serde_json::json!({
                    "ask_user_question": {
                        "id": "q_fixture",
                        "title": "Tabs or spaces?",
                        "options": ["tabs", "spaces"],
                    }
                }),
            },
        )
        .unwrap();

        let qid = QuestionId::new("q_fixture");
        store.ask(
            qid.clone(),
            "Tabs or spaces?".into(),
            vec!["tabs".into(), "spaces".into()],
            QuestionOrigin {
                session_id: Some(session.id),
                agent_group_id: Some(session.agent_group_id),
                message_out_id: Some(ask_row_id),
                channel_type: Some(ChannelType::new("cli")),
                platform_id: Some("stdin".into()),
                thread_id: None,
            },
            asked_at,
        );
        (qid, ask_seq)
    }

    fn expiry_notes(root: &MemSessionRoot, session: &Session) -> Vec<serde_json::Value> {
        let outbound = root
            .outbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        messages_out::list_due(outbound.conn())
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == MessageKind::System && r.content.get("edit").is_some())
            .map(|r| r.content)
            .collect()
    }

    fn result_rows(root: &MemSessionRoot, session: &Session) -> Vec<serde_json::Value> {
        let inbound = root
            .inbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        let mut stmt = inbound
            .conn()
            .prepare("SELECT content, trigger FROM messages_in WHERE kind = 'system'")
            .unwrap();
        stmt.query_map([], |row| {
            let content: String = row.get(0)?;
            let trigger: i64 = row.get(1)?;
            Ok(serde_json::json!({
                "content": serde_json::from_str::<serde_json::Value>(&content).unwrap(),
                "trigger": trigger,
            }))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    /// The full unanswered-expiry surface: one card-stamping edit note
    /// keyed at the ask row's seq, one `trigger = 0` synthetic result
    /// the agent's next turn will read, and the pending set drained.
    #[test]
    fn unanswered_question_past_ttl_is_surfaced_out_loud() {
        let (_c, root, session, now) = fixture();
        let store = InteractiveModule::default(); // 24h TTL
        let asked_at = now - ChDuration::hours(25);
        let (qid, ask_seq) = seed_asked_question(&root, &session, &store, asked_at);

        let emits = check(&root, &store, now);
        assert_eq!(emits.len(), 1);
        assert_eq!(emits[0].question_id, qid.0);
        assert_eq!(emits[0].session_id, Some(session.id));
        assert!(emits[0].note_emitted, "note must be emitted");
        assert!(!emits[0].resolved_by_reply);

        // Terminal note: an edit row stamping the original card.
        let notes = expiry_notes(&root, &session);
        assert_eq!(notes.len(), 1, "exactly one expiry note");
        assert_eq!(notes[0]["edit"]["seq"], serde_json::json!(ask_seq));
        assert_eq!(
            notes[0]["edit"]["text"],
            serde_json::json!(EXPIRED_QUESTION_TEXT)
        );

        // Synthetic no-answer result, non-triggering.
        let results = result_rows(&root, &session);
        assert_eq!(results.len(), 1, "exactly one synthetic result row");
        assert_eq!(results[0]["trigger"], 0, "must not spawn a container");
        let payload = &results[0]["content"]["ask_user_question_result"];
        assert_eq!(payload["id"], serde_json::json!(qid.0));
        assert_eq!(payload["status"], "expired");
        assert_eq!(payload["title"], "Tabs or spaces?");

        // Terminal in module state.
        assert!(store.pending().is_empty());
    }

    /// Second pass is byte-quiet: the module's sweep drained the
    /// question, so no duplicate note or result row appears.
    #[test]
    fn expiry_is_surfaced_exactly_once() {
        let (_c, root, session, now) = fixture();
        let store = InteractiveModule::default();
        let asked_at = now - ChDuration::hours(25);
        let _ = seed_asked_question(&root, &session, &store, asked_at);

        assert_eq!(check(&root, &store, now).len(), 1);
        assert!(check(&root, &store, now).is_empty(), "second pass quiet");
        assert_eq!(expiry_notes(&root, &session).len(), 1);
        assert_eq!(result_rows(&root, &session).len(), 1);
    }

    /// A question still inside its TTL is untouched entirely.
    #[test]
    fn question_inside_ttl_is_untouched() {
        let (_c, root, session, now) = fixture();
        let store = InteractiveModule::default();
        let asked_at = now - ChDuration::hours(23);
        let _ = seed_asked_question(&root, &session, &store, asked_at);

        assert!(check(&root, &store, now).is_empty());
        assert!(expiry_notes(&root, &session).is_empty());
        assert!(result_rows(&root, &session).is_empty());
        assert_eq!(store.pending().len(), 1, "still pending");
    }

    /// The answered-later gate: a chat inbound newer than the ask means
    /// the user replied (option tap or free text), so the lapse resolves
    /// silently — no note, no synthetic result.
    #[test]
    fn question_answered_by_later_reply_is_resolved_silently() {
        let (_c, root, session, now) = fixture();
        let store = InteractiveModule::default();
        let asked_at = now - ChDuration::hours(25);
        let _ = seed_asked_question(&root, &session, &store, asked_at);

        // The user's reply, an hour after the ask.
        let inbound = root
            .inbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        messages_in::insert(
            inbound.conn(),
            &messages_in::WriteInbound {
                id: MessageId::new(),
                kind: MessageKind::Chat,
                timestamp: asked_at + ChDuration::hours(1),
                content: serde_json::json!({"text": "spaces"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        drop(inbound);

        let emits = check(&root, &store, now);
        assert_eq!(emits.len(), 1);
        assert!(emits[0].resolved_by_reply, "reply gate must fire");
        assert!(!emits[0].note_emitted, "no note for an answered question");
        assert!(expiry_notes(&root, &session).is_empty());
        assert!(result_rows(&root, &session).is_empty());
        assert!(store.pending().is_empty(), "still terminal in state");
    }

    /// A question explicitly marked answered through the module API is
    /// never selected at all (the module-level guard).
    #[test]
    fn module_answered_question_is_never_selected() {
        let (_c, root, session, now) = fixture();
        let store = InteractiveModule::default();
        let asked_at = now - ChDuration::hours(25);
        let (qid, _) = seed_asked_question(&root, &session, &store, asked_at);
        assert!(store.answer(&qid, "tabs".into()));

        assert!(check(&root, &store, now).is_empty());
        assert!(expiry_notes(&root, &session).is_empty());
        assert_eq!(store.pending().len(), 1, "answered question stays put");
    }

    /// No origin at all: the lapse is recorded but nothing is written.
    #[test]
    fn expired_question_without_origin_records_lapse_only() {
        let (_c, root, _session, now) = fixture();
        let store = InteractiveModule::default();
        store.ask(
            QuestionId::new("q_orphan"),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            now - ChDuration::hours(25),
        );

        let emits = check(&root, &store, now);
        assert_eq!(emits.len(), 1);
        assert_eq!(emits[0].session_id, None);
        assert!(!emits[0].note_emitted);
        assert!(!emits[0].resolved_by_reply);
    }

    /// Missing channel routing: the synthetic result is still written
    /// (the agent must be unblocked) but no outbound note is emitted.
    #[test]
    fn expired_question_without_routing_skips_note_but_unblocks_agent() {
        let (_c, root, session, now) = fixture();
        let store = InteractiveModule::default();
        store.ask(
            QuestionId::new("q_routeless"),
            "pick".into(),
            vec!["a".into()],
            QuestionOrigin {
                session_id: Some(session.id),
                agent_group_id: Some(session.agent_group_id),
                message_out_id: None,
                channel_type: None,
                platform_id: None,
                thread_id: None,
            },
            now - ChDuration::hours(25),
        );

        let emits = check(&root, &store, now);
        assert_eq!(emits.len(), 1);
        assert!(!emits[0].note_emitted);
        assert!(expiry_notes(&root, &session).is_empty());
        assert_eq!(result_rows(&root, &session).len(), 1);
    }

    /// A DB failure surfacing one question is logged and recorded, not
    /// propagated — the sweep pass must not abort.
    #[test]
    fn surfacing_failure_is_swallowed() {
        let (_c, _root, session, now) = fixture();
        let store = InteractiveModule::default();
        store.ask(
            QuestionId::new("q_broken"),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin {
                session_id: Some(session.id),
                agent_group_id: Some(session.agent_group_id),
                message_out_id: None,
                channel_type: Some(ChannelType::new("cli")),
                platform_id: Some("stdin".into()),
                thread_id: None,
            },
            now - ChDuration::hours(25),
        );

        let strict = MemSessionRoot::new_strict_unknown();
        let emits = check(&strict, &store, now);
        assert_eq!(emits.len(), 1);
        assert!(!emits[0].note_emitted);
        assert!(store.pending().is_empty(), "question still drained");
    }

    /// Copy discipline: user-facing, no operator jargon, actionable.
    #[test]
    fn expiry_copy_matches_house_style() {
        assert!(EXPIRED_QUESTION_TEXT.contains("expired"));
        assert!(
            EXPIRED_QUESTION_TEXT.to_lowercase().contains("just reply"),
            "copy must tell the user the one useful next step",
        );
        for jargon in ["sweep", "TTL", "seq", "operator"] {
            assert!(
                !EXPIRED_QUESTION_TEXT.contains(jargon),
                "user copy must not leak {jargon}",
            );
        }
    }
}
