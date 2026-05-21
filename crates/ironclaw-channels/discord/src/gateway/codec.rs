//! Pure WS-frame parser for the Discord gateway protocol.
//!
//! Discord frames have the shape `{ "op": u8, "d": Value, "s": Option<u64>,
//! "t": Option<String> }`. This module turns a `&str` (or `&Value`) into a
//! `GatewayFrame` enum so the lifecycle logic stays testable without
//! touching a real WebSocket.
//!
//! Only the opcodes Ironclaw actually drives are spelled out; everything
//! else lands in `GatewayFrame::Other` so unexpected payloads don't panic
//! us mid-stream.

use ironclaw_channels_core::AdapterError;
use serde_json::Value;

/// Discord gateway opcodes (subset).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    Dispatch = 0,
    Heartbeat = 1,
    Identify = 2,
    Resume = 6,
    Reconnect = 7,
    InvalidSession = 9,
    Hello = 10,
    HeartbeatAck = 11,
}

impl Opcode {
    /// Decode a raw `op` integer into a known opcode.
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Dispatch),
            1 => Some(Self::Heartbeat),
            2 => Some(Self::Identify),
            6 => Some(Self::Resume),
            7 => Some(Self::Reconnect),
            9 => Some(Self::InvalidSession),
            10 => Some(Self::Hello),
            11 => Some(Self::HeartbeatAck),
            _ => None,
        }
    }

    /// Encode an opcode back to its raw integer.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A parsed gateway frame, narrowed to what the adapter actually handles.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayFrame {
    /// `op: 10` — `{ heartbeat_interval: ms }`.
    Hello { heartbeat_interval_ms: u64 },
    /// `op: 11` — server acked our heartbeat.
    HeartbeatAck,
    /// `op: 1` — server asked us to heartbeat immediately. Optional `s`.
    HeartbeatRequest { last_sequence: Option<u64> },
    /// `op: 7` — server wants us to reconnect (resumable).
    Reconnect,
    /// `op: 9` — `d: bool` resumable flag.
    InvalidSession { resumable: bool },
    /// `op: 0` — a dispatched event. `t` is the event name, `d` the payload.
    Dispatch {
        event: String,
        sequence: u64,
        data: Value,
    },
    /// An opcode we know about but don't act on, or a payload missing fields.
    Other { op: u8, data: Value },
}

/// Parse a raw text frame from the gateway into a `GatewayFrame`.
pub fn parse_frame(text: &str) -> Result<GatewayFrame, AdapterError> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| AdapterError::Transport(format!("gateway: invalid JSON ({e})")))?;
    parse_value(&v)
}

/// Parse a `serde_json::Value` (already deserialized) into a `GatewayFrame`.
pub fn parse_value(v: &Value) -> Result<GatewayFrame, AdapterError> {
    let op_raw = v
        .get("op")
        .and_then(Value::as_u64)
        .ok_or_else(|| AdapterError::Transport("gateway: missing `op`".into()))?;
    let op_u8 = u8::try_from(op_raw).map_err(|_| {
        AdapterError::Transport(format!("gateway: op {op_raw} out of u8 range"))
    })?;
    let data = v.get("d").cloned().unwrap_or(Value::Null);
    let sequence = v.get("s").and_then(Value::as_u64);
    let event = v
        .get("t")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let Some(op) = Opcode::from_u8(op_u8) else {
        return Ok(GatewayFrame::Other { op: op_u8, data });
    };

    match op {
        Opcode::Hello => {
            let ms = data
                .get("heartbeat_interval")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    AdapterError::Transport("HELLO missing heartbeat_interval".into())
                })?;
            Ok(GatewayFrame::Hello {
                heartbeat_interval_ms: ms,
            })
        }
        Opcode::HeartbeatAck => Ok(GatewayFrame::HeartbeatAck),
        Opcode::Heartbeat => Ok(GatewayFrame::HeartbeatRequest {
            last_sequence: sequence,
        }),
        Opcode::Reconnect => Ok(GatewayFrame::Reconnect),
        Opcode::InvalidSession => Ok(GatewayFrame::InvalidSession {
            resumable: data.as_bool().unwrap_or(false),
        }),
        Opcode::Dispatch => {
            let seq = sequence.ok_or_else(|| {
                AdapterError::Transport("DISPATCH frame missing `s`".into())
            })?;
            let name = event.ok_or_else(|| {
                AdapterError::Transport("DISPATCH frame missing `t`".into())
            })?;
            Ok(GatewayFrame::Dispatch {
                event: name,
                sequence: seq,
                data,
            })
        }
        Opcode::Identify | Opcode::Resume => Ok(GatewayFrame::Other { op: op_u8, data }),
    }
}

/// Build an outgoing `IDENTIFY` frame body (`op: 2`).
pub fn identify_payload(token: &str, intents: u64) -> Value {
    serde_json::json!({
        "op": Opcode::Identify.as_u8(),
        "d": {
            "token": token,
            "intents": intents,
            "properties": {
                "os": std::env::consts::OS,
                "browser": "ironclaw",
                "device": "ironclaw",
            },
        }
    })
}

/// Build an outgoing `RESUME` frame body (`op: 6`).
pub fn resume_payload(token: &str, session_id: &str, sequence: u64) -> Value {
    serde_json::json!({
        "op": Opcode::Resume.as_u8(),
        "d": {
            "token": token,
            "session_id": session_id,
            "seq": sequence,
        }
    })
}

/// Build an outgoing `HEARTBEAT` frame body (`op: 1`). `last_sequence` is
/// `null` if we've not yet seen any DISPATCH.
pub fn heartbeat_payload(last_sequence: Option<u64>) -> Value {
    serde_json::json!({
        "op": Opcode::Heartbeat.as_u8(),
        "d": last_sequence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn opcode_roundtrip_known() {
        for op in [
            Opcode::Dispatch,
            Opcode::Heartbeat,
            Opcode::Identify,
            Opcode::Resume,
            Opcode::Reconnect,
            Opcode::InvalidSession,
            Opcode::Hello,
            Opcode::HeartbeatAck,
        ] {
            assert_eq!(Opcode::from_u8(op.as_u8()), Some(op));
        }
    }

    #[test]
    fn opcode_unknown_is_none() {
        assert!(Opcode::from_u8(99).is_none());
    }

    #[test]
    fn parses_hello() {
        let f = parse_frame(r#"{"op":10,"d":{"heartbeat_interval":41250}}"#).unwrap();
        assert_eq!(
            f,
            GatewayFrame::Hello {
                heartbeat_interval_ms: 41_250,
            }
        );
    }

    #[test]
    fn hello_missing_interval_errors() {
        let err = parse_frame(r#"{"op":10,"d":{}}"#).unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn parses_heartbeat_ack() {
        let f = parse_frame(r#"{"op":11,"d":null}"#).unwrap();
        assert_eq!(f, GatewayFrame::HeartbeatAck);
    }

    #[test]
    fn parses_heartbeat_request_with_sequence() {
        let f = parse_frame(r#"{"op":1,"d":null,"s":42}"#).unwrap();
        assert_eq!(
            f,
            GatewayFrame::HeartbeatRequest {
                last_sequence: Some(42),
            }
        );
    }

    #[test]
    fn parses_reconnect() {
        let f = parse_frame(r#"{"op":7,"d":null}"#).unwrap();
        assert_eq!(f, GatewayFrame::Reconnect);
    }

    #[test]
    fn parses_invalid_session_resumable_true() {
        let f = parse_frame(r#"{"op":9,"d":true}"#).unwrap();
        assert_eq!(f, GatewayFrame::InvalidSession { resumable: true });
    }

    #[test]
    fn parses_invalid_session_resumable_false_default() {
        let f = parse_frame(r#"{"op":9,"d":null}"#).unwrap();
        assert_eq!(f, GatewayFrame::InvalidSession { resumable: false });
    }

    #[test]
    fn parses_ready_dispatch() {
        let f = parse_frame(
            r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"sess","resume_gateway_url":"wss://x","user":{"id":"bot"}}}"#,
        )
        .unwrap();
        match f {
            GatewayFrame::Dispatch {
                event,
                sequence,
                data,
            } => {
                assert_eq!(event, "READY");
                assert_eq!(sequence, 1);
                assert_eq!(data["session_id"], "sess");
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn parses_message_create_dispatch() {
        let f = parse_frame(
            r#"{"op":0,"t":"MESSAGE_CREATE","s":7,"d":{"id":"m1","channel_id":"c1"}}"#,
        )
        .unwrap();
        match f {
            GatewayFrame::Dispatch { event, sequence, .. } => {
                assert_eq!(event, "MESSAGE_CREATE");
                assert_eq!(sequence, 7);
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_missing_sequence_errors() {
        let err = parse_frame(r#"{"op":0,"t":"READY","d":{}}"#).unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn dispatch_missing_event_errors() {
        let err = parse_frame(r#"{"op":0,"s":1,"d":{}}"#).unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn missing_op_field_errors() {
        let err = parse_frame(r#"{"d":null}"#).unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn out_of_range_op_errors() {
        let err = parse_frame(r#"{"op":999,"d":null}"#).unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn unknown_op_becomes_other() {
        let f = parse_frame(r#"{"op":42,"d":{"k":"v"}}"#).unwrap();
        assert_eq!(
            f,
            GatewayFrame::Other {
                op: 42,
                data: json!({"k": "v"}),
            }
        );
    }

    #[test]
    fn outbound_identify_outbound_resume_outbound_heartbeat() {
        let id = identify_payload("tok", 33_281);
        assert_eq!(id["op"], 2);
        assert_eq!(id["d"]["token"], "tok");
        assert_eq!(id["d"]["intents"], 33_281);
        assert!(id["d"]["properties"].is_object());

        let res = resume_payload("tok", "sess", 99);
        assert_eq!(res["op"], 6);
        assert_eq!(res["d"]["session_id"], "sess");
        assert_eq!(res["d"]["seq"], 99);

        let hb = heartbeat_payload(Some(5));
        assert_eq!(hb["op"], 1);
        assert_eq!(hb["d"], 5);

        let hb_null = heartbeat_payload(None);
        assert!(hb_null["d"].is_null());
    }

    #[test]
    fn invalid_json_errors() {
        let err = parse_frame("not json").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn parse_value_matches_parse_frame() {
        let txt = r#"{"op":11,"d":null}"#;
        let v: Value = serde_json::from_str(txt).unwrap();
        assert_eq!(parse_value(&v).unwrap(), parse_frame(txt).unwrap());
    }

    #[test]
    fn opcode_identify_and_resume_when_received_become_other() {
        // The server never sends these, but we still want them to round-trip
        // gracefully if they appear.
        let f = parse_frame(r#"{"op":2,"d":null}"#).unwrap();
        assert!(matches!(f, GatewayFrame::Other { op: 2, .. }));
        let f = parse_frame(r#"{"op":6,"d":null}"#).unwrap();
        assert!(matches!(f, GatewayFrame::Other { op: 6, .. }));
    }
}
