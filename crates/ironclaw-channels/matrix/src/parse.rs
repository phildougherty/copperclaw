//! Pure functions that turn a `/sync` response into [`InboundEvent`]s.
//!
//! These helpers do not touch HTTP or the filesystem so they can be unit-
//! tested in isolation with fixture JSON.

use chrono::{DateTime, TimeZone, Utc};
use ironclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity,
};
use serde_json::{Value, json};

use crate::factory::CHANNEL_TYPE_STR;

/// Convert a `/sync` response into a flat list of [`InboundEvent`]s.
///
/// `bot_user_id` is the bot's MXID; events authored by that user are
/// filtered out (a Matrix bot would otherwise loop on its own sends).
pub fn sync_to_events(sync: &Value, bot_user_id: &str) -> Vec<InboundEvent> {
    let mut out = Vec::new();
    let Some(rooms) = sync.get("rooms").and_then(|v| v.get("join")) else {
        return out;
    };
    let Some(rooms) = rooms.as_object() else {
        return out;
    };
    for (room_id, room) in rooms {
        let Some(events) = room
            .get("timeline")
            .and_then(|t| t.get("events"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for event in events {
            if let Some(evt) = event_to_inbound(room_id, event, bot_user_id) {
                out.push(evt);
            }
        }
    }
    out
}

/// Build an [`InboundEvent`] from a single timeline event JSON object.
/// Returns `None` for unsupported event types or events authored by the bot.
pub fn event_to_inbound(
    room_id: &str,
    event: &Value,
    bot_user_id: &str,
) -> Option<InboundEvent> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    if event_type != "m.room.message" {
        return None;
    }
    let sender = event.get("sender").and_then(Value::as_str)?;
    if sender == bot_user_id {
        return None;
    }
    let content = event.get("content")?;
    let msgtype = content.get("msgtype").and_then(Value::as_str)?;

    let event_id = event
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let ts_ms = event
        .get("origin_server_ts")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let channel_type = ChannelType::new(CHANNEL_TYPE_STR);

    let (kind, payload) = match msgtype {
        "m.text" => {
            let text = content
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (MessageKind::Chat, json!({ "text": text }))
        }
        "m.file" | "m.image" | "m.audio" | "m.video" => {
            let body = content
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let url = content
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mime = content
                .get("info")
                .and_then(|i| i.get("mimetype"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            (
                MessageKind::Chat,
                json!({
                    "text": body,
                    "attachment": {
                        "url": url,
                        "mimetype": mime,
                        "msgtype": msgtype,
                    }
                }),
            )
        }
        _ => return None,
    };

    let thread_id = content
        .get("m.relates_to")
        .and_then(|r| {
            let rel_type = r.get("rel_type").and_then(Value::as_str)?;
            if rel_type == "m.thread" {
                r.get("event_id").and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            }
        });

    let is_mention = Some(detect_mention(content, bot_user_id));

    Some(InboundEvent {
        channel_type: channel_type.clone(),
        platform_id: room_id.to_owned(),
        thread_id,
        message: InboundMessage {
            id: event_id,
            kind,
            content: payload,
            timestamp: ms_to_datetime(ts_ms),
            is_mention,
            is_group: Some(true),
        },
        reply_to: None,
        sender: Some(SenderIdentity {
            channel_type,
            identity: sender.to_owned(),
            display_name: None,
        }),
    })
}

/// Detect whether the event mentions the bot.
///
/// First checks the modern `m.mentions.user_ids` array; if absent or
/// empty, falls back to a case-insensitive substring search on the bot's
/// MXID in the message body.
pub fn detect_mention(content: &Value, bot_user_id: &str) -> bool {
    if let Some(arr) = content
        .get("m.mentions")
        .and_then(|m| m.get("user_ids"))
        .and_then(Value::as_array)
    {
        if arr.iter().any(|v| v.as_str() == Some(bot_user_id)) {
            return true;
        }
        if !arr.is_empty() {
            return false;
        }
    }
    let body = content
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    body.to_ascii_lowercase()
        .contains(&bot_user_id.to_ascii_lowercase())
}

/// Read `next_batch` from a `/sync` response. Returns `None` if missing.
pub fn next_batch_of(sync: &Value) -> Option<&str> {
    sync.get("next_batch").and_then(Value::as_str)
}

fn ms_to_datetime(ms: i64) -> DateTime<Utc> {
    let secs = ms.div_euclid(1000);
    let nanos = u32::try_from(ms.rem_euclid(1000).unsigned_abs() * 1_000_000).unwrap_or(0);
    match Utc.timestamp_opt(secs, nanos) {
        chrono::LocalResult::Single(dt) => dt,
        _ => Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message_event(
        sender: &str,
        body: &str,
        extra: Option<Value>,
    ) -> Value {
        let mut content = json!({
            "msgtype": "m.text",
            "body": body,
        });
        if let Some(extra) = extra {
            if let (Some(obj), Some(other)) = (content.as_object_mut(), extra.as_object()) {
                for (k, v) in other {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        json!({
            "type": "m.room.message",
            "event_id": "$e:m.org",
            "sender": sender,
            "origin_server_ts": 1_000,
            "content": content,
        })
    }

    fn sync_with(room: &str, events: &[Value]) -> Value {
        json!({
            "next_batch": "next-1",
            "rooms": {
                "join": {
                    room: {
                        "timeline": {
                            "events": events,
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn text_message_becomes_chat_event() {
        let s = sync_with("!a:m.org", &[message_event("@alice:m.org", "hello", None)]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts.len(), 1);
        let e = &evts[0];
        assert_eq!(e.channel_type.as_str(), "matrix");
        assert_eq!(e.platform_id, "!a:m.org");
        assert_eq!(e.message.kind, MessageKind::Chat);
        assert_eq!(e.message.content["text"], "hello");
        assert_eq!(e.message.is_group, Some(true));
        let sender = e.sender.as_ref().expect("sender");
        assert_eq!(sender.identity, "@alice:m.org");
        assert_eq!(sender.channel_type.as_str(), "matrix");
    }

    #[test]
    fn threaded_message_extracts_thread_id() {
        let extra = json!({
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": "$root:m.org"
            }
        });
        let s = sync_with(
            "!a:m.org",
            &[message_event("@alice:m.org", "reply", Some(extra))],
        );
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts[0].thread_id.as_deref(), Some("$root:m.org"));
    }

    #[test]
    fn non_thread_relation_has_no_thread_id() {
        let extra = json!({
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": "$old:m.org"
            }
        });
        let s = sync_with(
            "!a:m.org",
            &[message_event("@alice:m.org", "edit", Some(extra))],
        );
        let evts = sync_to_events(&s, "@bot:m.org");
        assert!(evts[0].thread_id.is_none());
    }

    #[test]
    fn file_message_carries_attachment_metadata() {
        let event = json!({
            "type": "m.room.message",
            "event_id": "$f:m.org",
            "sender": "@alice:m.org",
            "origin_server_ts": 1_000,
            "content": {
                "msgtype": "m.file",
                "body": "doc.txt",
                "filename": "doc.txt",
                "info": { "mimetype": "text/plain", "size": 3 },
                "url": "mxc://m.org/abc"
            }
        });
        let s = sync_with("!a:m.org", &[event]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].message.kind, MessageKind::Chat);
        assert_eq!(evts[0].message.content["text"], "doc.txt");
        assert_eq!(evts[0].message.content["attachment"]["url"], "mxc://m.org/abc");
        assert_eq!(evts[0].message.content["attachment"]["mimetype"], "text/plain");
        assert_eq!(evts[0].message.content["attachment"]["msgtype"], "m.file");
    }

    #[test]
    fn image_message_recognised() {
        let event = json!({
            "type": "m.room.message",
            "event_id": "$i:m.org",
            "sender": "@alice:m.org",
            "origin_server_ts": 1_000,
            "content": {
                "msgtype": "m.image",
                "body": "p.png",
                "info": { "mimetype": "image/png" },
                "url": "mxc://m.org/p"
            }
        });
        let s = sync_with("!a:m.org", &[event]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].message.content["attachment"]["msgtype"], "m.image");
    }

    #[test]
    fn audio_and_video_messages_recognised() {
        for msgtype in ["m.audio", "m.video"] {
            let event = json!({
                "type": "m.room.message",
                "event_id": "$x:m.org",
                "sender": "@alice:m.org",
                "origin_server_ts": 1_000,
                "content": {
                    "msgtype": msgtype,
                    "body": "x",
                    "info": { "mimetype": "application/octet-stream" },
                    "url": "mxc://m.org/x"
                }
            });
            let s = sync_with("!a:m.org", &[event]);
            let evts = sync_to_events(&s, "@bot:m.org");
            assert_eq!(evts.len(), 1);
            assert_eq!(evts[0].message.content["attachment"]["msgtype"], msgtype);
        }
    }

    #[test]
    fn bot_own_event_is_filtered() {
        let s = sync_with("!a:m.org", &[message_event("@bot:m.org", "self", None)]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert!(evts.is_empty());
    }

    #[test]
    fn non_message_event_is_ignored() {
        let event = json!({
            "type": "m.room.member",
            "event_id": "$x:m.org",
            "sender": "@alice:m.org",
            "origin_server_ts": 1_000,
            "content": {}
        });
        let s = sync_with("!a:m.org", &[event]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert!(evts.is_empty());
    }

    #[test]
    fn unknown_msgtype_is_ignored() {
        let event = json!({
            "type": "m.room.message",
            "event_id": "$x:m.org",
            "sender": "@alice:m.org",
            "origin_server_ts": 1_000,
            "content": { "msgtype": "m.sticker", "body": "x" }
        });
        let s = sync_with("!a:m.org", &[event]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert!(evts.is_empty());
    }

    #[test]
    fn event_missing_sender_is_ignored() {
        let event = json!({
            "type": "m.room.message",
            "event_id": "$x:m.org",
            "origin_server_ts": 1_000,
            "content": { "msgtype": "m.text", "body": "x" }
        });
        let s = sync_with("!a:m.org", &[event]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert!(evts.is_empty());
    }

    #[test]
    fn event_missing_content_is_ignored() {
        let event = json!({
            "type": "m.room.message",
            "event_id": "$x:m.org",
            "sender": "@alice:m.org",
            "origin_server_ts": 1_000,
        });
        let s = sync_with("!a:m.org", &[event]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert!(evts.is_empty());
    }

    #[test]
    fn m_mentions_user_ids_marks_mention() {
        let extra = json!({
            "m.mentions": {
                "user_ids": ["@bot:m.org"]
            }
        });
        let s = sync_with(
            "!a:m.org",
            &[message_event("@alice:m.org", "hi", Some(extra))],
        );
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts[0].message.is_mention, Some(true));
    }

    #[test]
    fn m_mentions_user_ids_without_bot_is_not_mention() {
        let extra = json!({
            "m.mentions": { "user_ids": ["@other:m.org"] }
        });
        let s = sync_with(
            "!a:m.org",
            &[message_event("@alice:m.org", "hi", Some(extra))],
        );
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts[0].message.is_mention, Some(false));
    }

    #[test]
    fn substring_mention_fallback_matches_body() {
        let s = sync_with(
            "!a:m.org",
            &[message_event("@alice:m.org", "Hello @bot:m.org!", None)],
        );
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts[0].message.is_mention, Some(true));
    }

    #[test]
    fn substring_mention_fallback_case_insensitive() {
        let s = sync_with(
            "!a:m.org",
            &[message_event("@alice:m.org", "Hello @BOT:M.ORG!", None)],
        );
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts[0].message.is_mention, Some(true));
    }

    #[test]
    fn substring_mention_fallback_no_match() {
        let s = sync_with(
            "!a:m.org",
            &[message_event("@alice:m.org", "nothing here", None)],
        );
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts[0].message.is_mention, Some(false));
    }

    #[test]
    fn empty_m_mentions_array_falls_through_to_substring() {
        // An empty m.mentions.user_ids array should not preempt substring
        // search; falling back lets the legacy "@bot in body" path work.
        let extra = json!({ "m.mentions": { "user_ids": [] } });
        let s = sync_with(
            "!a:m.org",
            &[message_event("@alice:m.org", "hi @bot:m.org", Some(extra))],
        );
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts[0].message.is_mention, Some(true));
    }

    #[test]
    fn sync_missing_rooms_yields_nothing() {
        let evts = sync_to_events(&json!({"next_batch": "x"}), "@bot:m.org");
        assert!(evts.is_empty());
    }

    #[test]
    fn sync_missing_timeline_yields_nothing() {
        let s = json!({
            "next_batch": "x",
            "rooms": { "join": { "!a:m.org": {} } }
        });
        let evts = sync_to_events(&s, "@bot:m.org");
        assert!(evts.is_empty());
    }

    #[test]
    fn sync_join_not_object_yields_nothing() {
        let s = json!({
            "next_batch": "x",
            "rooms": { "join": "junk" }
        });
        let evts = sync_to_events(&s, "@bot:m.org");
        assert!(evts.is_empty());
    }

    #[test]
    fn next_batch_present() {
        let s = json!({ "next_batch": "abc" });
        assert_eq!(next_batch_of(&s), Some("abc"));
    }

    #[test]
    fn next_batch_missing() {
        assert!(next_batch_of(&json!({})).is_none());
    }

    #[test]
    fn ms_to_datetime_zero() {
        let dt = ms_to_datetime(0);
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn ms_to_datetime_falls_back_for_extreme_inputs() {
        let _ = ms_to_datetime(i64::MAX);
    }

    #[test]
    fn detect_mention_handles_missing_body_and_mentions() {
        assert!(!detect_mention(&json!({}), "@bot:m.org"));
        // body present, no mention
        assert!(!detect_mention(&json!({"body": "hi"}), "@bot:m.org"));
    }

    #[test]
    fn detect_mention_invalid_m_mentions_falls_back_to_substring() {
        // m.mentions present but not the right shape -> falls back.
        assert!(detect_mention(
            &json!({"m.mentions": "junk", "body": "hi @bot:m.org"}),
            "@bot:m.org"
        ));
    }

    #[test]
    fn timestamp_propagates_from_origin_server_ts() {
        let event = json!({
            "type": "m.room.message",
            "event_id": "$e:m.org",
            "sender": "@alice:m.org",
            "origin_server_ts": 1_700_000_000_000_i64,
            "content": { "msgtype": "m.text", "body": "hi" }
        });
        let s = sync_with("!a:m.org", &[event]);
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts[0].message.timestamp.timestamp(), 1_700_000_000);
    }

    #[test]
    fn multiple_rooms_emit_multiple_events() {
        let s = json!({
            "next_batch": "x",
            "rooms": {
                "join": {
                    "!a:m.org": {
                        "timeline": { "events": [
                            message_event("@alice:m.org", "in a", None)
                        ]}
                    },
                    "!b:m.org": {
                        "timeline": { "events": [
                            message_event("@bob:m.org", "in b", None)
                        ]}
                    }
                }
            }
        });
        let evts = sync_to_events(&s, "@bot:m.org");
        assert_eq!(evts.len(), 2);
    }
}
