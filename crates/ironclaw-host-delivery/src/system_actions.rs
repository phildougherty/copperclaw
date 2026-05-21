//! Parsing for `MessageKind::System` outbound rows.
//!
//! A system row carries a JSON object whose first top-level key names a
//! registered delivery action and whose value is the action's payload. For
//! example:
//!
//! ```json
//! { "approve_sender": { "user_id": "u_..." } }
//! ```
//!
//! Names beginning with `_` are reserved for the runtime (e.g. compaction
//! markers) and are skipped silently.

use crate::error::DeliveryError;

/// Parsed action payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAction {
    pub name: String,
    pub payload: serde_json::Value,
}

/// Parse a `MessageKind::System` row's `content` blob into an action name and
/// payload. Returns `Ok(None)` when the row is well-formed but doesn't carry
/// any actionable top-level key (private `_*` keys, empty object).
pub fn parse_system_content(
    content: &serde_json::Value,
) -> Result<Option<ParsedAction>, DeliveryError> {
    let obj = content.as_object().ok_or_else(|| {
        DeliveryError::SystemAction(format!(
            "system content must be a JSON object, got {}",
            type_name(content)
        ))
    })?;

    for (key, value) in obj {
        if key.starts_with('_') {
            continue;
        }
        return Ok(Some(ParsedAction {
            name: key.clone(),
            payload: value.clone(),
        }));
    }
    Ok(None)
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_first_top_level_key() {
        let v = json!({ "approve_sender": { "user": "u" } });
        let action = parse_system_content(&v).unwrap().unwrap();
        assert_eq!(action.name, "approve_sender");
        assert_eq!(action.payload, json!({ "user": "u" }));
    }

    #[test]
    fn skips_underscore_prefixed_keys() {
        let v = json!({ "_meta": true, "do_thing": { "x": 1 } });
        let action = parse_system_content(&v).unwrap().unwrap();
        assert_eq!(action.name, "do_thing");
    }

    #[test]
    fn returns_none_when_all_keys_private() {
        let v = json!({ "_meta": true });
        assert_eq!(parse_system_content(&v).unwrap(), None);
    }

    #[test]
    fn returns_none_on_empty_object() {
        let v = json!({});
        assert_eq!(parse_system_content(&v).unwrap(), None);
    }

    #[test]
    fn errors_on_non_object_array() {
        let v = json!([1, 2, 3]);
        let err = parse_system_content(&v).unwrap_err();
        assert!(matches!(err, DeliveryError::SystemAction(msg) if msg.contains("array")));
    }

    #[test]
    fn errors_on_non_object_string() {
        let v = json!("hello");
        let err = parse_system_content(&v).unwrap_err();
        assert!(matches!(err, DeliveryError::SystemAction(msg) if msg.contains("string")));
    }

    #[test]
    fn errors_on_non_object_null() {
        let v = json!(null);
        let err = parse_system_content(&v).unwrap_err();
        assert!(matches!(err, DeliveryError::SystemAction(msg) if msg.contains("null")));
    }

    #[test]
    fn errors_on_non_object_bool() {
        let v = json!(true);
        let err = parse_system_content(&v).unwrap_err();
        assert!(matches!(err, DeliveryError::SystemAction(msg) if msg.contains("bool")));
    }

    #[test]
    fn errors_on_non_object_number() {
        let v = json!(7);
        let err = parse_system_content(&v).unwrap_err();
        assert!(matches!(err, DeliveryError::SystemAction(msg) if msg.contains("number")));
    }

    #[test]
    fn type_name_covers_every_variant() {
        assert_eq!(type_name(&json!(null)), "null");
        assert_eq!(type_name(&json!(true)), "bool");
        assert_eq!(type_name(&json!(1)), "number");
        assert_eq!(type_name(&json!("s")), "string");
        assert_eq!(type_name(&json!([])), "array");
        assert_eq!(type_name(&json!({})), "object");
    }
}
