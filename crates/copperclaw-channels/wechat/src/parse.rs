//! Inbound XML parsing for Work Weixin callbacks.
//!
//! Work Weixin pushes inbound events as a small XML envelope. After
//! decryption (see [`crate::signature::decrypt_payload`]) the inner XML
//! payload looks like:
//!
//! ```xml
//! <xml>
//!   <ToUserName><![CDATA[wx-corp]]></ToUserName>
//!   <FromUserName><![CDATA[user1]]></FromUserName>
//!   <CreateTime>1700000000</CreateTime>
//!   <MsgType><![CDATA[text]]></MsgType>
//!   <Content><![CDATA[hello]]></Content>
//!   <MsgId>1234567890</MsgId>
//!   <AgentID>1000002</AgentID>
//! </xml>
//! ```
//!
//! Other `MsgType` values carry different payload fields. We model the
//! common ones explicitly and fall back to capturing the raw envelope as
//! a JSON map for unknown shapes.
//!
//! The parser is intentionally hand-rolled — Work Weixin XML is shallow,
//! one-level, and bringing a general XML parser into the dependency tree
//! is overkill. We accept the CDATA-wrapped and plain forms; we do not
//! support nested elements (none of the documented event shapes need
//! them).

use thiserror::Error;

/// Recognised `MsgType` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    /// Text message.
    Text,
    /// Image attachment.
    Image,
    /// Voice attachment.
    Voice,
    /// Video attachment.
    Video,
    /// File / document attachment.
    File,
    /// Platform event (subscribe, click, etc).
    Event,
    /// Anything else.
    Other,
}

impl MsgType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Voice => "voice",
            Self::Video => "video",
            Self::File => "file",
            Self::Event => "event",
            Self::Other => "other",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "text" => Self::Text,
            "image" => Self::Image,
            "voice" => Self::Voice,
            "video" => Self::Video,
            "file" => Self::File,
            "event" => Self::Event,
            _ => Self::Other,
        }
    }
}

/// Parsed inbound XML envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundXml {
    /// Receiver (the agent's user-side corpid). Mostly cosmetic on inbound.
    pub to_user_name: String,
    /// Sender userid — the corp user who sent the message.
    pub from_user_name: String,
    /// Unix-second event time as the wire integer (we keep it as a string
    /// because the platform sometimes emits it that way).
    pub create_time: String,
    /// `MsgType`.
    pub msg_type: MsgType,
    /// Raw `MsgType` string as it appeared on the wire.
    pub msg_type_raw: String,
    /// `Content` body (text messages only). Empty for non-text events.
    pub content: String,
    /// `MsgId` — used for duplicate suppression.
    pub msg_id: Option<String>,
    /// `MediaId` for attachments.
    pub media_id: Option<String>,
    /// `Event` discriminator for `MsgType == event` callbacks.
    pub event: Option<String>,
    /// `EventKey` for `event` callbacks (e.g. click menu key).
    pub event_key: Option<String>,
    /// `AgentID` the message was addressed to.
    pub agent_id: Option<String>,
    /// `Format` (file/voice/video extension hint).
    pub format: Option<String>,
    /// `FileName` for file attachments.
    pub file_name: Option<String>,
}

/// Reason XML parsing failed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// The body did not contain a `<xml>` root element.
    #[error("missing <xml> root element")]
    NoRoot,
    /// A required field (`FromUserName` or `MsgType`) was missing.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
}

/// Parse a Work Weixin inbound XML payload.
///
/// Tolerant of element order. CDATA wrappers are stripped. Unknown
/// elements are ignored.
pub fn parse_inbound_xml(body: &[u8]) -> Result<InboundXml, ParseError> {
    let s = std::str::from_utf8(body).map_err(|_| ParseError::NoRoot)?;
    let root_open = s
        .find("<xml>")
        .or_else(|| s.find("<xml "))
        .ok_or(ParseError::NoRoot)?;
    let root_close = s.rfind("</xml>").ok_or(ParseError::NoRoot)?;
    if root_close <= root_open {
        return Err(ParseError::NoRoot);
    }
    // Body between the root tags. We don't need the `<xml>` markers
    // themselves.
    let inner_start = root_open + "<xml>".len();
    let inner = &s[inner_start..root_close];

    let to_user_name = extract_field(inner, "ToUserName").unwrap_or_default();
    let from_user_name =
        extract_field(inner, "FromUserName").ok_or(ParseError::MissingField("FromUserName"))?;
    let msg_type_raw =
        extract_field(inner, "MsgType").ok_or(ParseError::MissingField("MsgType"))?;
    let msg_type = MsgType::parse(&msg_type_raw);
    let create_time = extract_field(inner, "CreateTime").unwrap_or_default();
    let content = extract_field(inner, "Content").unwrap_or_default();
    let msg_id = extract_field(inner, "MsgId");
    let media_id = extract_field(inner, "MediaId");
    let event = extract_field(inner, "Event");
    let event_key = extract_field(inner, "EventKey");
    let agent_id = extract_field(inner, "AgentID");
    let format = extract_field(inner, "Format");
    let file_name = extract_field(inner, "FileName");

    Ok(InboundXml {
        to_user_name,
        from_user_name,
        create_time,
        msg_type,
        msg_type_raw,
        content,
        msg_id,
        media_id,
        event,
        event_key,
        agent_id,
        format,
        file_name,
    })
}

/// Extract a single element's text body from a shallow XML snippet.
fn extract_field(haystack: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = haystack.find(&open)?;
    let after_open = start + open.len();
    let end = haystack[after_open..].find(&close)?;
    let raw = &haystack[after_open..after_open + end];
    Some(strip_cdata(raw).to_owned())
}

fn strip_cdata(s: &str) -> &str {
    let trimmed = s.trim();
    let cdata_open = "<![CDATA[";
    let cdata_close = "]]>";
    if let Some(rest) = trimmed.strip_prefix(cdata_open) {
        if let Some(inner) = rest.strip_suffix(cdata_close) {
            return inner;
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_message() {
        let body = r"<xml>
            <ToUserName><![CDATA[wx-corp]]></ToUserName>
            <FromUserName><![CDATA[user1]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[text]]></MsgType>
            <Content><![CDATA[hello world]]></Content>
            <MsgId>1234567890</MsgId>
            <AgentID>1000002</AgentID>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.to_user_name, "wx-corp");
        assert_eq!(parsed.from_user_name, "user1");
        assert_eq!(parsed.create_time, "1700000000");
        assert_eq!(parsed.msg_type, MsgType::Text);
        assert_eq!(parsed.msg_type_raw, "text");
        assert_eq!(parsed.content, "hello world");
        assert_eq!(parsed.msg_id.as_deref(), Some("1234567890"));
        assert_eq!(parsed.agent_id.as_deref(), Some("1000002"));
    }

    #[test]
    fn parses_image_message() {
        let body = r"<xml>
            <FromUserName><![CDATA[user1]]></FromUserName>
            <MsgType><![CDATA[image]]></MsgType>
            <MediaId><![CDATA[MEDIA-IMG]]></MediaId>
            <PicUrl><![CDATA[https://example.test/pic.jpg]]></PicUrl>
            <MsgId>2</MsgId>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.msg_type, MsgType::Image);
        assert_eq!(parsed.media_id.as_deref(), Some("MEDIA-IMG"));
    }

    #[test]
    fn parses_voice_message() {
        let body = r"<xml>
            <FromUserName><![CDATA[user1]]></FromUserName>
            <MsgType><![CDATA[voice]]></MsgType>
            <MediaId><![CDATA[MEDIA-VOICE]]></MediaId>
            <Format><![CDATA[amr]]></Format>
            <MsgId>3</MsgId>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.msg_type, MsgType::Voice);
        assert_eq!(parsed.media_id.as_deref(), Some("MEDIA-VOICE"));
        assert_eq!(parsed.format.as_deref(), Some("amr"));
    }

    #[test]
    fn parses_video_message() {
        let body = r"<xml>
            <FromUserName><![CDATA[user1]]></FromUserName>
            <MsgType><![CDATA[video]]></MsgType>
            <MediaId><![CDATA[MEDIA-VID]]></MediaId>
            <MsgId>4</MsgId>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.msg_type, MsgType::Video);
        assert_eq!(parsed.media_id.as_deref(), Some("MEDIA-VID"));
    }

    #[test]
    fn parses_file_message() {
        let body = r"<xml>
            <FromUserName><![CDATA[user1]]></FromUserName>
            <MsgType><![CDATA[file]]></MsgType>
            <MediaId><![CDATA[MEDIA-FILE]]></MediaId>
            <FileName><![CDATA[report.pdf]]></FileName>
            <MsgId>5</MsgId>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.msg_type, MsgType::File);
        assert_eq!(parsed.media_id.as_deref(), Some("MEDIA-FILE"));
        assert_eq!(parsed.file_name.as_deref(), Some("report.pdf"));
    }

    #[test]
    fn parses_event_message() {
        let body = r"<xml>
            <FromUserName><![CDATA[user1]]></FromUserName>
            <MsgType><![CDATA[event]]></MsgType>
            <Event><![CDATA[subscribe]]></Event>
            <EventKey><![CDATA[]]></EventKey>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.msg_type, MsgType::Event);
        assert_eq!(parsed.event.as_deref(), Some("subscribe"));
        assert!(parsed.event_key.is_some());
    }

    #[test]
    fn parses_event_with_key() {
        let body = r"<xml>
            <FromUserName><![CDATA[user1]]></FromUserName>
            <MsgType><![CDATA[event]]></MsgType>
            <Event><![CDATA[click]]></Event>
            <EventKey><![CDATA[K42]]></EventKey>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.event.as_deref(), Some("click"));
        assert_eq!(parsed.event_key.as_deref(), Some("K42"));
    }

    #[test]
    fn parses_other_msg_type_yields_other_variant() {
        let body = r"<xml>
            <FromUserName><![CDATA[user1]]></FromUserName>
            <MsgType><![CDATA[location]]></MsgType>
            <MsgId>6</MsgId>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.msg_type, MsgType::Other);
        assert_eq!(parsed.msg_type_raw, "location");
    }

    #[test]
    fn rejects_missing_xml_root() {
        let err = parse_inbound_xml(b"<not_xml>x</not_xml>").unwrap_err();
        assert_eq!(err, ParseError::NoRoot);
    }

    #[test]
    fn rejects_open_without_close() {
        let err = parse_inbound_xml(b"<xml>...").unwrap_err();
        assert_eq!(err, ParseError::NoRoot);
    }

    #[test]
    fn rejects_missing_from_user_name() {
        let body = r"<xml>
            <ToUserName><![CDATA[wx]]></ToUserName>
            <MsgType><![CDATA[text]]></MsgType>
        </xml>";
        let err = parse_inbound_xml(body.as_bytes()).unwrap_err();
        assert_eq!(err, ParseError::MissingField("FromUserName"));
    }

    #[test]
    fn rejects_missing_msg_type() {
        let body = r"<xml>
            <FromUserName><![CDATA[u]]></FromUserName>
        </xml>";
        let err = parse_inbound_xml(body.as_bytes()).unwrap_err();
        assert_eq!(err, ParseError::MissingField("MsgType"));
    }

    #[test]
    fn rejects_non_utf8_body() {
        let err = parse_inbound_xml(&[0xC3, 0x28]).unwrap_err();
        assert_eq!(err, ParseError::NoRoot);
    }

    #[test]
    fn handles_plain_field_without_cdata() {
        let body = r"<xml>
            <FromUserName>user1</FromUserName>
            <MsgType>text</MsgType>
            <Content>plain content</Content>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        assert_eq!(parsed.from_user_name, "user1");
        assert_eq!(parsed.content, "plain content");
    }

    #[test]
    fn extract_field_returns_none_when_missing() {
        assert!(extract_field("<a>x</a>", "B").is_none());
    }

    #[test]
    fn extract_field_unbalanced_close_returns_none() {
        assert!(extract_field("<A>x", "A").is_none());
    }

    #[test]
    fn strip_cdata_idempotent_on_plain_text() {
        assert_eq!(strip_cdata("hello"), "hello");
    }

    #[test]
    fn strip_cdata_trims_whitespace() {
        assert_eq!(strip_cdata("  hello  "), "hello");
    }

    #[test]
    fn strip_cdata_strips_wrappers() {
        assert_eq!(strip_cdata("<![CDATA[wrapped]]>"), "wrapped");
        assert_eq!(strip_cdata("  <![CDATA[wrapped]]>  "), "wrapped");
    }

    #[test]
    fn strip_cdata_leaves_unmatched_wrapper() {
        // Only an opening with no close — defensive: return trimmed
        // string.
        assert_eq!(strip_cdata("<![CDATA[no end"), "<![CDATA[no end");
    }

    #[test]
    fn msg_type_as_str_round_trips() {
        for raw in ["text", "image", "voice", "video", "file", "event", "weird"] {
            let mt = MsgType::parse(raw);
            let s = mt.as_str();
            // round-trip only for known variants
            if matches!(
                mt,
                MsgType::Text
                    | MsgType::Image
                    | MsgType::Voice
                    | MsgType::Video
                    | MsgType::File
                    | MsgType::Event
            ) {
                assert_eq!(MsgType::parse(s), mt);
            } else {
                assert_eq!(MsgType::parse(s), MsgType::Other);
            }
        }
    }

    #[test]
    fn parse_error_display_unique_per_variant() {
        let variants = [ParseError::NoRoot, ParseError::MissingField("X")];
        let mut seen = std::collections::HashSet::new();
        for v in &variants {
            assert!(seen.insert(format!("{v}")));
            let _ = format!("{v:?}");
        }
    }

    #[test]
    fn clone_eq_round_trip() {
        let body = r"<xml>
            <FromUserName>user1</FromUserName>
            <MsgType>text</MsgType>
            <Content>hi</Content>
        </xml>";
        let parsed = parse_inbound_xml(body.as_bytes()).unwrap();
        let c = parsed.clone();
        assert_eq!(parsed, c);
    }
}
