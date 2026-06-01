//! Telegram pairing wizard.
//!
//! Driven from [`super::channel::ChannelStep`] when the operator picks
//! `telegram` as the first channel. The wizard walks the user through:
//!
//! 1. Creating a bot via `@BotFather`.
//! 2. Pasting and validating the resulting HTTP API token (regex check
//!    plus a `getMe` round-trip).
//! 3. Optionally capturing the first chat id by polling `getUpdates`
//!    for the user's `/start`.
//! 4. Persisting the credentials into the `.env` written by the auth
//!    step.
//!
//! The same code path drives interactive and `--headless` runs; in the
//! headless case the answers come from `COPPERCLAW_SETUP_TELEGRAM_*`
//! variables. Token-bearing strings are never logged; the audit log
//! receives only the redacted form (`<digits>:****<last-4>`).
//!
//! TODO(team-d): generalize for other channels — Slack / Discord could
//! reuse `append_env_var` and the same `<verify_token, capture_chat_id>`
//! shape with channel-specific adapters.

use crate::prompt::Prompt;
use crate::state::write_secret_file;
use crate::steps::StepError;
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

/// Canonical name for the env var the Telegram channel adapter reads.
pub const TELEGRAM_BOT_TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";

/// Canonical name for the optional first chat id captured during pairing.
pub const TELEGRAM_CHAT_ID_ENV: &str = "TELEGRAM_CHAT_ID";

/// Headless prompt key for the bot token. Maps to
/// `COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN`.
pub const HEADLESS_TOKEN_KEY: &str = "TELEGRAM_BOT_TOKEN";

/// Headless prompt key for the chat id. Maps to
/// `COPPERCLAW_SETUP_TELEGRAM_CHAT_ID`.
pub const HEADLESS_CHAT_ID_KEY: &str = "TELEGRAM_CHAT_ID";

/// Production Telegram bot API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Per-request timeout for `getMe` verification.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Default budget for `/start` polling.
pub const CHAT_ID_POLL_BUDGET: Duration = Duration::from_secs(60);

/// Bot-info payload returned by `getMe`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BotInfo {
    /// Bot username (no leading `@`).
    pub username: String,
    /// Human-readable display name.
    pub first_name: String,
}

/// Outcome of the Telegram pairing flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOutcome {
    /// Validated token (only retained in memory long enough to write it
    /// to disk; never logged or echoed in messages).
    pub token: String,
    /// Bot username / `first_name` returned by `getMe`. `None` when the
    /// network call to Telegram failed and we stored the token without
    /// verification.
    pub bot_info: Option<BotInfo>,
    /// First chat id captured via `getUpdates`. `None` when the
    /// operator skipped or the budget elapsed.
    pub chat_id: Option<i64>,
    /// `true` when the `getMe` verification step failed (operator was
    /// warned, token was still stored).
    pub verification_skipped: bool,
}

/// Whether `token` matches the `BotFather` format (`^\d+:[A-Za-z0-9_-]+$`).
#[must_use]
pub fn is_valid_bot_token(token: &str) -> bool {
    let token = token.trim();
    let Some((digits, suffix)) = token.split_once(':') else {
        return false;
    };
    if digits.is_empty() || suffix.is_empty() {
        return false;
    }
    digits.bytes().all(|b| b.is_ascii_digit())
        && suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Redact a bot token for logging. Returns the leading numeric prefix
/// plus a four-character suffix masked with stars; falls back to
/// `<redacted>` when the input has no `:` separator.
#[must_use]
pub fn redact_token(token: &str) -> String {
    let token = token.trim();
    let Some((digits, suffix)) = token.split_once(':') else {
        return "<redacted>".to_string();
    };
    let tail = if suffix.len() >= 4 {
        &suffix[suffix.len() - 4..]
    } else {
        ""
    };
    format!("{digits}:****{tail}")
}

/// Async `getMe` round-trip.
///
/// Returns the parsed `BotInfo` on success, an error message otherwise.
/// The HTTP call is bounded to [`VERIFY_TIMEOUT`].
pub async fn verify_token(api_base: &str, token: &str) -> Result<BotInfo, String> {
    let url = format!("{}/bot{}/getMe", api_base.trim_end_matches('/'), token);
    let client = reqwest::Client::builder()
        .timeout(VERIFY_TIMEOUT)
        .build()
        .map_err(|e| format!("http client build: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    if !status.is_success() {
        if let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(desc) = envelope.get("description").and_then(|v| v.as_str()) {
                return Err(format!("telegram http {status}: {desc}"));
            }
        }
        return Err(format!("telegram http {status}"));
    }
    let envelope: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode response: {e}"))?;
    if envelope.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let desc = envelope
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("telegram returned ok=false");
        return Err(desc.to_string());
    }
    let result = envelope
        .get("result")
        .ok_or_else(|| "telegram response missing `result`".to_string())?;
    let username = result
        .get("username")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "telegram result missing `username`".to_string())?;
    let first_name = result
        .get("first_name")
        .and_then(|v| v.as_str())
        .unwrap_or(username);
    Ok(BotInfo {
        username: username.to_string(),
        first_name: first_name.to_string(),
    })
}

/// Synchronous wrapper around [`verify_token`].
///
/// Drives the future on the current Tokio runtime via
/// [`tokio::task::block_in_place`], matching the existing pattern in
/// [`super::onecli::run_probe`].
pub fn verify_token_blocking(api_base: &str, token: &str) -> Result<BotInfo, String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "no Tokio runtime available for telegram verify".to_string())?;
    let api_base = api_base.to_string();
    let token = token.to_string();
    tokio::task::block_in_place(|| {
        handle.block_on(async move { verify_token(&api_base, &token).await })
    })
}

/// Poll Telegram's `getUpdates` for the first incoming chat id.
///
/// Returns `Ok(Some(chat_id))` on success, `Ok(None)` if no message
/// arrives within `budget`, and `Err` on a hard failure (e.g. 401).
/// Transient HTTP failures inside the loop are swallowed and retried
/// until the budget elapses.
pub async fn poll_for_chat_id(
    api_base: &str,
    token: &str,
    budget: Duration,
) -> Result<Option<i64>, String> {
    let url = format!("{}/bot{}/getUpdates", api_base.trim_end_matches('/'), token);
    let client = reqwest::Client::builder()
        .timeout(VERIFY_TIMEOUT)
        .build()
        .map_err(|e| format!("http client build: {e}"))?;
    let deadline = Instant::now() + budget;
    let mut last_offset: i64 = 0;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_secs = remaining.as_secs().clamp(1, 5);
        let payload = serde_json::json!({
            "offset": last_offset,
            "timeout": timeout_secs,
            "limit": 5,
            "allowed_updates": ["message"]
        });
        let Ok(resp) = client.post(&url).json(&payload).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            if resp.status().as_u16() == 401 {
                return Err(format!("telegram auth failed (http {})", resp.status()));
            }
            continue;
        }
        let Ok(body) = resp.text().await else {
            continue;
        };
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        if envelope.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            continue;
        }
        let Some(updates) = envelope.get("result").and_then(|v| v.as_array()) else {
            continue;
        };
        for upd in updates {
            if let Some(id) = upd.get("update_id").and_then(serde_json::Value::as_i64) {
                last_offset = id + 1;
            }
            if let Some(chat_id) = upd
                .get("message")
                .and_then(|m| m.get("chat"))
                .and_then(|c| c.get("id"))
                .and_then(serde_json::Value::as_i64)
            {
                return Ok(Some(chat_id));
            }
        }
    }
    Ok(None)
}

/// Synchronous wrapper around [`poll_for_chat_id`].
pub fn poll_for_chat_id_blocking(
    api_base: &str,
    token: &str,
    budget: Duration,
) -> Result<Option<i64>, String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "no Tokio runtime available for telegram poll".to_string())?;
    let api_base = api_base.to_string();
    let token = token.to_string();
    tokio::task::block_in_place(|| {
        handle.block_on(async move { poll_for_chat_id(&api_base, &token, budget).await })
    })
}

/// Append (or replace) `KEY=value` in the `.env` at `path`.
///
/// Creates the file if missing. If `key` already appears, the existing
/// line is replaced in place so re-runs are idempotent. On Unix the
/// file is opened with mode `0o600` from the start — closing the
/// TOCTOU window where a chmod-after-write left the bot token
/// world-readable for a moment during initial install.
pub fn append_env_var(path: &Path, key: &str, value: &str) -> Result<(), StepError> {
    let new_line = format!("{key}={value}\n");
    let prefix = format!("{key}=");
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let mut replaced = false;
    let mut rebuilt = String::with_capacity(existing.len() + new_line.len());
    for line in existing.lines() {
        if line.starts_with(&prefix) {
            rebuilt.push_str(&new_line);
            replaced = true;
        } else {
            rebuilt.push_str(line);
            rebuilt.push('\n');
        }
    }
    if !replaced {
        if !rebuilt.is_empty() && !rebuilt.ends_with('\n') {
            rebuilt.push('\n');
        }
        rebuilt.push_str(&new_line);
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    write_secret_file(path, rebuilt.as_bytes())?;
    Ok(())
}

/// Lines printed before the `BotFather` walkthrough.
#[must_use]
pub fn intro_lines() -> Vec<String> {
    vec![
        "I'll help you create a Telegram bot.".to_string(),
        "You'll need: a phone with Telegram installed. This takes ~2 minutes.".to_string(),
        String::new(),
        "Open Telegram and message @BotFather.".to_string(),
        "Send `/newbot`. Choose a display name and a unique username ending in `bot`.".to_string(),
        "BotFather will reply with an HTTP API token like `123456:ABC-DEF...`.".to_string(),
    ]
}

/// Drive the interactive (or env-backed) Telegram pairing flow.
///
/// Returns `Ok(Some(outcome))` when pairing succeeded (possibly with
/// `verification_skipped=true` if `getMe` failed), `Ok(None)` when the
/// operator typed `skip`, or `Err` on a non-recoverable error such as
/// the env-var being unset in headless mode.
pub fn run_pairing(
    prompt: &dyn Prompt,
    api_base: &str,
    out: &mut Vec<String>,
) -> Result<Option<PairingOutcome>, StepError> {
    for line in intro_lines() {
        out.push(line);
    }

    let Some(token) = capture_token(prompt, out)? else {
        out.push("Telegram pairing skipped — wire it later via `cclaw channel ...`.".to_string());
        return Ok(None);
    };

    let (bot_info, verification_skipped) = match verify_token_blocking(api_base, &token) {
        Ok(info) => {
            out.push(format!(
                "Connected to @{} ('{}'). Looks good.",
                info.username, info.first_name
            ));
            (Some(info), false)
        }
        Err(e) => {
            out.push(format!(
                "WARN: could not verify token ({e}). Storing it anyway — re-run setup to re-check."
            ));
            (None, true)
        }
    };

    let chat_id = capture_chat_id(prompt, api_base, &token, out)?;

    let bot_handle = bot_info.as_ref().map(|b| b.username.clone());
    push_success_footer(out, bot_handle.as_deref(), chat_id);
    Ok(Some(PairingOutcome {
        token,
        bot_info,
        chat_id,
        verification_skipped,
    }))
}

fn capture_token(prompt: &dyn Prompt, out: &mut Vec<String>) -> Result<Option<String>, StepError> {
    // Track the previous invalid response so we can detect a headless
    // (`EnvBacked`) prompt that keeps returning the same malformed
    // value on every iteration. Without this guard, a malformed
    // `COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN` would spin the loop forever
    // because env-backed `secret()` is purely deterministic.
    let mut prev_invalid: Option<String> = None;
    loop {
        let raw = prompt.secret(HEADLESS_TOKEN_KEY, "Paste the token here (or type `skip`)")?;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("skip") {
            return Ok(None);
        }
        if is_valid_bot_token(trimmed) {
            out.push(format!(
                "Token format looks right ({}).",
                redact_token(trimmed)
            ));
            return Ok(Some(trimmed.to_string()));
        }
        // Headless-loop guard: if we already saw this exact invalid
        // value once, a second identical response means the prompt
        // can't yield anything different (env-backed) — bail with a
        // clear error rather than spin forever.
        if prev_invalid.as_deref() == Some(trimmed) {
            return Err(StepError::Other(format!(
                "COPPERCLAW_SETUP_{HEADLESS_TOKEN_KEY} failed bot-token validation; \
                 expected format `<bot_id>:<token>` (got {})",
                redact_token(trimmed)
            )));
        }
        prev_invalid = Some(trimmed.to_string());
        out.push(
            "That doesn't look like a BotFather token (expected `<digits>:<chars>`). Try again."
                .to_string(),
        );
    }
}

fn capture_chat_id(
    prompt: &dyn Prompt,
    api_base: &str,
    token: &str,
    out: &mut Vec<String>,
) -> Result<Option<i64>, StepError> {
    // Headless override wins outright.
    if let Ok(raw) = prompt.input(HEADLESS_CHAT_ID_KEY, "", Some("")) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return if let Ok(n) = trimmed.parse::<i64>() {
                out.push(format!("Using pre-supplied chat id {n}."));
                Ok(Some(n))
            } else {
                out.push(format!(
                    "Pre-supplied {HEADLESS_CHAT_ID_KEY} is not numeric — skipping chat-id step."
                ));
                Ok(None)
            };
        }
    }

    let want_capture = prompt.confirm(
        "TELEGRAM_CAPTURE_CHAT_ID",
        "Send `/start` to your bot now so I can capture the first chat id? (y/N, ~60s)",
        false,
    )?;
    if !want_capture {
        out.push(
            "Skipping chat-id capture — you can wire one later with `cclaw messaging-groups ...`."
                .to_string(),
        );
        return Ok(None);
    }

    out.push("Waiting up to 60s for a message from you to the bot...".to_string());
    match poll_for_chat_id_blocking(api_base, token, CHAT_ID_POLL_BUDGET) {
        Ok(Some(id)) => {
            out.push(format!("Captured chat id {id}."));
            Ok(Some(id))
        }
        Ok(None) => {
            out.push("No message arrived in time — skipping chat-id capture.".to_string());
            Ok(None)
        }
        Err(e) => {
            out.push(format!("Chat-id capture failed: {e}."));
            Ok(None)
        }
    }
}

fn push_success_footer(out: &mut Vec<String>, bot_handle: Option<&str>, chat_id: Option<i64>) {
    out.push(String::new());
    let handle_hint = bot_handle.map(|h| format!(" @{h}")).unwrap_or_default();
    out.push(format!(
        "Telegram is wired. Run `copperclaw run && cclaw chat`, or just message your bot at{handle_hint} directly."
    ));
    if chat_id.is_some() {
        out.push(
            "First chat id stored as `TELEGRAM_CHAT_ID` in the .env — the host will wire it to the default agent group on first boot.".to_string()
        );
    }
    out.push("To unwire later: `cclaw wirings delete <id>`.".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::{EnvBacked, Scripted};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ---- regex / redaction ----

    #[test]
    fn token_regex_accepts_canonical() {
        assert!(is_valid_bot_token(
            "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"
        ));
        assert!(is_valid_bot_token("1:A"));
    }

    #[test]
    fn token_regex_rejects_garbage() {
        assert!(!is_valid_bot_token(""));
        assert!(!is_valid_bot_token("abc"));
        assert!(!is_valid_bot_token("123456"));
        assert!(!is_valid_bot_token("123:abc!def"));
        assert!(!is_valid_bot_token(":abc"));
        assert!(!is_valid_bot_token("abc:def"));
        assert!(!is_valid_bot_token("123:"));
    }

    #[test]
    fn token_regex_trims_whitespace() {
        assert!(is_valid_bot_token("  1234:abc  "));
    }

    #[test]
    fn redact_token_masks_body() {
        let r = redact_token("12345:secretvalue");
        assert!(r.starts_with("12345:****"));
        assert!(r.ends_with("alue"));
        assert!(!r.contains("secret"));
    }

    #[test]
    fn redact_token_short_suffix() {
        let r = redact_token("12345:ab");
        assert_eq!(r, "12345:****");
    }

    #[test]
    fn redact_token_no_colon_returns_placeholder() {
        assert_eq!(redact_token("oops"), "<redacted>");
    }

    // ---- verify_token (wiremock) ----

    async fn server() -> MockServer {
        MockServer::start().await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_token_success_decodes_username() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "id": 1, "is_bot": true, "username": "ironbot", "first_name": "Iron" }
            })))
            .mount(&s)
            .await;
        let info = verify_token(&s.uri(), "tok").await.unwrap();
        assert_eq!(info.username, "ironbot");
        assert_eq!(info.first_name, "Iron");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_token_first_name_falls_back_to_username() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "id": 1, "is_bot": true, "username": "ironbot" }
            })))
            .mount(&s)
            .await;
        let info = verify_token(&s.uri(), "tok").await.unwrap();
        assert_eq!(info.first_name, "ironbot");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_token_401_surfaces_description() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false, "error_code": 401, "description": "Unauthorized"
            })))
            .mount(&s)
            .await;
        let err = verify_token(&s.uri(), "tok").await.unwrap_err();
        assert!(err.contains("Unauthorized"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_token_ok_false_is_error() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false, "description": "bad"
            })))
            .mount(&s)
            .await;
        let err = verify_token(&s.uri(), "tok").await.unwrap_err();
        assert_eq!(err, "bad");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_token_missing_result_is_error() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true
            })))
            .mount(&s)
            .await;
        let err = verify_token(&s.uri(), "tok").await.unwrap_err();
        assert!(err.contains("missing"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_token_network_failure_returns_error() {
        let err = verify_token("http://127.0.0.1:1", "tok").await.unwrap_err();
        assert!(!err.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_token_blocking_succeeds_under_runtime() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "id": 1, "is_bot": true, "username": "ironbot" }
            })))
            .mount(&s)
            .await;
        let uri = s.uri();
        let info = verify_token_blocking(&uri, "tok").unwrap();
        assert_eq!(info.username, "ironbot");
    }

    // ---- poll_for_chat_id ----

    #[tokio::test(flavor = "multi_thread")]
    async fn poll_for_chat_id_returns_chat_id_on_message() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": [
                    {
                        "update_id": 7,
                        "message": {
                            "message_id": 1,
                            "date": 1,
                            "chat": { "id": 4242, "type": "private" },
                            "text": "/start"
                        }
                    }
                ]
            })))
            .mount(&s)
            .await;
        let got = poll_for_chat_id(&s.uri(), "tok", Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(got, Some(4242));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn poll_for_chat_id_returns_none_when_no_update_in_budget() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": []
            })))
            .mount(&s)
            .await;
        let got = poll_for_chat_id(&s.uri(), "tok", Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(got, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn poll_for_chat_id_returns_err_on_401() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&s)
            .await;
        let err = poll_for_chat_id(&s.uri(), "tok", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(err.contains("auth"));
    }

    // ---- append_env_var ----

    #[test]
    fn append_env_var_creates_file_when_missing() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".env");
        append_env_var(&p, "TELEGRAM_BOT_TOKEN", "123:abc").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert_eq!(body, "TELEGRAM_BOT_TOKEN=123:abc\n");
    }

    #[test]
    fn append_env_var_appends_to_existing_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "ANTHROPIC_API_KEY=sk\n").unwrap();
        append_env_var(&p, "TELEGRAM_BOT_TOKEN", "123:abc").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("ANTHROPIC_API_KEY=sk\n"));
        assert!(body.contains("TELEGRAM_BOT_TOKEN=123:abc\n"));
    }

    #[test]
    fn append_env_var_replaces_existing_key() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "TELEGRAM_BOT_TOKEN=old\nOTHER=keep\n").unwrap();
        append_env_var(&p, "TELEGRAM_BOT_TOKEN", "new").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("TELEGRAM_BOT_TOKEN=new\n"));
        assert!(!body.contains("TELEGRAM_BOT_TOKEN=old"));
        assert!(body.contains("OTHER=keep\n"));
    }

    #[test]
    fn append_env_var_handles_file_without_trailing_newline() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "ANTHROPIC_API_KEY=sk").unwrap();
        append_env_var(&p, "K", "v").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("ANTHROPIC_API_KEY=sk\n"));
        assert!(body.ends_with("K=v\n"));
    }

    #[cfg(unix)]
    #[test]
    fn append_env_var_preserves_restrictive_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let p = dir.path().join(".env");
        append_env_var(&p, "K", "v").unwrap();
        let perms = std::fs::metadata(&p).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    // ---- intro / footer ----

    #[test]
    fn intro_lines_mentions_botfather() {
        let lines = intro_lines();
        assert!(lines.iter().any(|l| l.contains("@BotFather")));
        assert!(lines.iter().any(|l| l.contains("/newbot")));
    }

    // ---- run_pairing (scripted / env-backed) ----

    #[test]
    fn pairing_skip_token_returns_none() {
        let prompt = Scripted::new().with(HEADLESS_TOKEN_KEY, "skip");
        let mut out = Vec::new();
        let res = run_pairing(&prompt, "http://127.0.0.1:1", &mut out).unwrap();
        assert!(res.is_none());
        assert!(out.iter().any(|m| m.contains("skipped")));
    }

    #[test]
    fn pairing_invalid_token_then_skip() {
        let prompt = Scripted::new()
            .with(HEADLESS_TOKEN_KEY, "garbage")
            .with(HEADLESS_TOKEN_KEY, "skip");
        let mut out = Vec::new();
        let res = run_pairing(&prompt, "http://127.0.0.1:1", &mut out).unwrap();
        assert!(res.is_none());
        assert!(out.iter().any(|m| m.contains("doesn't look like")));
    }

    #[test]
    fn pairing_headless_malformed_token_bails_instead_of_looping() {
        // Regression: `EnvBacked` returns the same value on every
        // `secret()` call. Before this fix, a malformed
        // `COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN` would spin the loop
        // forever. Now the second identical-invalid response surfaces
        // a clear error.
        let mut env = HashMap::new();
        env.insert(
            EnvBacked::var_name(HEADLESS_TOKEN_KEY),
            "not-a-real-token".to_string(),
        );
        let prompt = EnvBacked::with_env(env);
        let mut out = Vec::new();
        let err = run_pairing(&prompt, "http://127.0.0.1:1", &mut out).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN") && msg.contains("validation"),
            "error should name the env var and the failure (got: {msg})"
        );
    }

    #[test]
    fn pairing_scripted_two_identical_invalids_bails() {
        // Same guard exercised through the scripted prompt: two
        // identical invalid values back-to-back means there's no way
        // for the loop to ever exit successfully.
        let prompt = Scripted::new()
            .with(HEADLESS_TOKEN_KEY, "rubbish")
            .with(HEADLESS_TOKEN_KEY, "rubbish");
        let mut out = Vec::new();
        let err = run_pairing(&prompt, "http://127.0.0.1:1", &mut out).unwrap_err();
        assert!(err.to_string().contains("validation"), "got: {err}");
    }

    #[test]
    fn pairing_scripted_two_different_invalids_then_skip_does_not_bail() {
        // Distinct invalid attempts shouldn't trip the guard — the
        // operator might be fat-fingering a paste. Only the
        // identical-repeat case is a headless dead-end.
        let prompt = Scripted::new()
            .with(HEADLESS_TOKEN_KEY, "garbage-one")
            .with(HEADLESS_TOKEN_KEY, "garbage-two")
            .with(HEADLESS_TOKEN_KEY, "skip");
        let mut out = Vec::new();
        let res = run_pairing(&prompt, "http://127.0.0.1:1", &mut out).unwrap();
        assert!(res.is_none(), "should reach the skip path");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_success_with_mock_getme() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "id": 1, "is_bot": true, "username": "ironbot", "first_name": "Iron" }
            })))
            .mount(&s)
            .await;
        let uri = s.uri();
        let outcome = tokio::task::spawn_blocking(move || {
            let prompt = Scripted::new()
                .with(HEADLESS_TOKEN_KEY, "123:abc")
                .with("TELEGRAM_CAPTURE_CHAT_ID", "no");
            let mut local = Vec::new();
            let r = run_pairing(&prompt, &uri, &mut local).unwrap();
            (r, local)
        })
        .await
        .unwrap();
        let outcome_unwrapped = outcome.0.expect("pairing succeeded");
        assert_eq!(outcome_unwrapped.token, "123:abc");
        assert_eq!(
            outcome_unwrapped
                .bot_info
                .as_ref()
                .map(|b| b.username.as_str()),
            Some("ironbot")
        );
        assert!(outcome_unwrapped.chat_id.is_none());
        assert!(!outcome_unwrapped.verification_skipped);
        assert!(outcome.1.iter().any(|m| m.contains("ironbot")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_verification_failure_is_soft() {
        let outcome = tokio::task::spawn_blocking(|| {
            let prompt = Scripted::new()
                .with(HEADLESS_TOKEN_KEY, "123:abc")
                .with("TELEGRAM_CAPTURE_CHAT_ID", "no");
            let mut out = Vec::new();
            run_pairing(&prompt, "http://127.0.0.1:1", &mut out).unwrap()
        })
        .await
        .unwrap()
        .expect("token retained even when verify fails");
        assert!(outcome.verification_skipped);
        assert_eq!(outcome.token, "123:abc");
        assert!(outcome.bot_info.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_headless_supplies_chat_id() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "id": 1, "is_bot": true, "username": "ironbot", "first_name": "Iron" }
            })))
            .mount(&s)
            .await;
        let uri = s.uri();
        let outcome = tokio::task::spawn_blocking(move || {
            let mut env = HashMap::new();
            env.insert(
                EnvBacked::var_name(HEADLESS_TOKEN_KEY),
                "123:abc".to_string(),
            );
            env.insert(
                EnvBacked::var_name(HEADLESS_CHAT_ID_KEY),
                "9988".to_string(),
            );
            let prompt = EnvBacked::with_env(env);
            let mut out = Vec::new();
            run_pairing(&prompt, &uri, &mut out).unwrap()
        })
        .await
        .unwrap()
        .expect("pairing succeeded");
        assert_eq!(outcome.chat_id, Some(9988));
    }

    #[test]
    fn pairing_outcome_round_trip_smoke() {
        let a = PairingOutcome {
            token: "t".into(),
            bot_info: Some(BotInfo {
                username: "u".into(),
                first_name: "n".into(),
            }),
            chat_id: Some(1),
            verification_skipped: false,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn env_var_constants_match_documented_names() {
        assert_eq!(TELEGRAM_BOT_TOKEN_ENV, "TELEGRAM_BOT_TOKEN");
        assert_eq!(TELEGRAM_CHAT_ID_ENV, "TELEGRAM_CHAT_ID");
        assert_eq!(HEADLESS_TOKEN_KEY, "TELEGRAM_BOT_TOKEN");
        assert_eq!(HEADLESS_CHAT_ID_KEY, "TELEGRAM_CHAT_ID");
    }

    #[test]
    fn default_api_base_is_production() {
        assert_eq!(DEFAULT_API_BASE, "https://api.telegram.org");
    }

    #[test]
    fn append_env_var_pathbuf_smoke() {
        let dir = tempdir().unwrap();
        let p: PathBuf = dir.path().join(".env");
        append_env_var(p.as_path(), "K", "v").unwrap();
        assert!(p.exists());
    }
}
