//! Production [`IMessageBridge`] backed by real `osascript` + `sqlite3`
//! subprocesses.
//!
//! This module is **macOS-only**: it lives behind `#[cfg(target_os =
//! "macos")]` so workspace-wide builds on Linux / Windows CI runners still
//! pass. Everything an adapter unit-test relies on lives in
//! [`crate::bridge::MockBridge`] instead.

use crate::bridge::{IMessageBridge, MockMessageRow};
use crate::config::IMessageConfig;
use crate::parse::select_new_rows_sql;
use async_trait::async_trait;
use copperclaw_channels_core::AdapterError;
use std::process::Stdio;
use tokio::process::Command;

/// Production [`IMessageBridge`].
#[derive(Debug, Clone)]
pub struct OsaScriptBridge {
    osascript_bin: String,
    sqlite3_bin: String,
    chat_db_path: std::path::PathBuf,
}

impl OsaScriptBridge {
    /// Construct a bridge from a parsed config.
    pub fn from_config(cfg: &IMessageConfig) -> Self {
        Self {
            osascript_bin: cfg.osascript_bin.clone(),
            sqlite3_bin: cfg.sqlite3_bin.clone(),
            chat_db_path: cfg.chat_db_path.clone(),
        }
    }

    /// Planned argv for `osascript -e <script>` (without the actual
    /// invocation). Exposed for tests that want to assert the command
    /// shape without spawning.
    pub fn plan_osascript_args(&self, script: &str) -> Vec<String> {
        vec!["-e".to_owned(), script.to_owned()]
    }

    /// Planned argv for `sqlite3 <db> <sql>` (without `?` substituted). The
    /// `?` placeholder is replaced by the actual `since_rowid` literal at
    /// call time.
    pub fn plan_sqlite3_args(&self, since_rowid: i64) -> Vec<String> {
        let sql = sqlite3_query_for(since_rowid);
        vec![
            "-separator".to_owned(),
            SQLITE_SEPARATOR.to_owned(),
            "-newline".to_owned(),
            SQLITE_NEWLINE.to_owned(),
            self.chat_db_path.to_string_lossy().into_owned(),
            sql,
        ]
    }
}

/// Field separator used between sqlite3 columns. Chosen so the byte cannot
/// appear in any normal field value.
pub const SQLITE_SEPARATOR: &str = "\x1f"; // ASCII Unit Separator.

/// Row separator used between sqlite3 rows.
pub const SQLITE_NEWLINE: &str = "\x1e"; // ASCII Record Separator.

/// Build the parameterised sqlite3 query with the `since_rowid` substituted
/// directly (sqlite3 CLI has no positional binding).
pub fn sqlite3_query_for(since_rowid: i64) -> String {
    select_new_rows_sql().replace("> ?", &format!("> {since_rowid}"))
}

#[async_trait]
impl IMessageBridge for OsaScriptBridge {
    async fn run_applescript(&self, script: &str) -> Result<String, AdapterError> {
        let mut cmd = Command::new(&self.osascript_bin);
        cmd.arg("-e").arg(script);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let output = cmd.output().await.map_err(|e| {
            AdapterError::Transport(format!(
                "spawn `{program}` failed: {e}",
                program = self.osascript_bin,
            ))
        })?;
        classify_osascript_output(&output.status, &output.stdout, &output.stderr)
    }

    async fn query_new_rows(&self, since_rowid: i64) -> Result<Vec<MockMessageRow>, AdapterError> {
        let args = self.plan_sqlite3_args(since_rowid);
        let mut cmd = Command::new(&self.sqlite3_bin);
        for arg in &args {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let output = cmd.output().await.map_err(|e| {
            AdapterError::Transport(format!(
                "spawn `{program}` failed: {e}",
                program = self.sqlite3_bin,
            ))
        })?;
        if !output.status.success() {
            let err_text = String::from_utf8_lossy(&output.stderr);
            return Err(classify_sqlite3_error(&err_text));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_sqlite_rows(&stdout))
    }
}

/// Turn `osascript` output into either stdout-as-string or a typed
/// [`AdapterError`].
///
/// AppleScript errors look like:
///
/// ```text
/// 0:0: execution error: Messages got an error: Can't get buddy "+nope" of service id <X>. (-1728)
/// ```
///
/// We map the broad families:
///
/// - "not authorised" / "not allowed assistive access" → [`AdapterError::Auth`]
/// - everything else → [`AdapterError::Transport`]
pub fn classify_osascript_output(
    status: &std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<String, AdapterError> {
    if status.success() {
        return Ok(String::from_utf8_lossy(stdout).into_owned());
    }
    let stderr_str = String::from_utf8_lossy(stderr);
    let lc = stderr_str.to_lowercase();
    if lc.contains("not authorised")
        || lc.contains("not authorized")
        || lc.contains("not allowed assistive access")
        || lc.contains("user has declined permission")
    {
        return Err(AdapterError::Auth(format!(
            "imessage: osascript not authorised: {}",
            stderr_str.trim()
        )));
    }
    Err(AdapterError::Transport(format!(
        "imessage: osascript exited {status}: {}",
        stderr_str.trim()
    )))
}

/// Classify a `sqlite3` failure. The most common one in practice is
/// `unable to open database file` when the host doesn't have Full Disk
/// Access permissions; we lift that to [`AdapterError::Auth`].
pub fn classify_sqlite3_error(stderr: &str) -> AdapterError {
    let lc = stderr.to_lowercase();
    if lc.contains("unable to open") || lc.contains("authorization denied") {
        return AdapterError::Auth(format!(
            "imessage: chat.db unreachable (Full Disk Access?): {}",
            stderr.trim()
        ));
    }
    AdapterError::Transport(format!("imessage: sqlite3 failed: {}", stderr.trim()))
}

/// Parse the raw `sqlite3` output our [`plan_sqlite3_args`] produces.
///
/// We pass `-separator \x1f -newline \x1e`, so each row is a `\x1e`-
/// terminated string of `\x1f`-separated columns matching the order in
/// [`select_new_rows_sql`].
///
/// Columns: `ROWID, guid, text, date, is_from_me, handle_id, handle, chat_id`.
///
/// Rows that fail to parse are skipped (logged at warn).
pub fn parse_sqlite_rows(stdout: &str) -> Vec<MockMessageRow> {
    let mut out = Vec::new();
    for raw in stdout.split(SQLITE_NEWLINE) {
        let row = raw.trim_matches('\n');
        if row.is_empty() {
            continue;
        }
        let cols: Vec<&str> = row.split(SQLITE_SEPARATOR).collect();
        if cols.len() < 8 {
            tracing::warn!(?cols, "imessage: malformed sqlite3 row; skipping");
            continue;
        }
        let rowid: i64 = match cols[0].parse() {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, value = %cols[0], "imessage: bad ROWID");
                continue;
            }
        };
        let guid = cols[1].to_owned();
        let text = if cols[2].is_empty() {
            None
        } else {
            Some(cols[2].to_owned())
        };
        let date: i64 = cols[3].parse().unwrap_or(0);
        let is_from_me = cols[4] != "0";
        // cols[5] is handle_id (numeric); skipped — we use the joined
        // `handle.id` at cols[6].
        let handle = if cols[6].is_empty() {
            None
        } else {
            Some(cols[6].to_owned())
        };
        let chat_id = if cols[7].is_empty() {
            None
        } else {
            Some(cols[7].to_owned())
        };
        out.push(MockMessageRow {
            rowid,
            guid,
            text,
            date,
            is_from_me,
            handle,
            chat_id,
        });
    }
    out
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn cfg_with(osa: &str, sql: &str, db: &str) -> IMessageConfig {
        let mut c = IMessageConfig::default();
        c.osascript_bin = osa.into();
        c.sqlite3_bin = sql.into();
        c.chat_db_path = db.into();
        c
    }

    #[test]
    fn plan_osascript_args_is_dash_e_then_script() {
        let b = OsaScriptBridge::from_config(&cfg_with("osascript", "sqlite3", "/db"));
        let args = b.plan_osascript_args("(foo)");
        assert_eq!(args, vec!["-e", "(foo)"]);
    }

    #[test]
    fn plan_sqlite3_args_includes_separator_newline_db_and_sql() {
        let b = OsaScriptBridge::from_config(&cfg_with("osascript", "sqlite3", "/path/chat.db"));
        let args = b.plan_sqlite3_args(123);
        assert!(args.contains(&"-separator".to_string()));
        assert!(args.contains(&SQLITE_SEPARATOR.to_string()));
        assert!(args.contains(&"-newline".to_string()));
        assert!(args.contains(&SQLITE_NEWLINE.to_string()));
        assert!(args.contains(&"/path/chat.db".to_string()));
        // The substituted SQL is the last arg.
        let sql = args.last().unwrap();
        assert!(sql.contains("> 123"));
    }

    #[test]
    fn sqlite3_query_for_substitutes_rowid_literal() {
        let q = sqlite3_query_for(99);
        assert!(q.contains("> 99"));
        // The placeholder must be gone.
        assert!(!q.contains("> ?"));
    }

    #[test]
    fn classify_osascript_success_returns_stdout() {
        let s = fake_status(0);
        let r = classify_osascript_output(&s, b"ok", b"").unwrap();
        assert_eq!(r, "ok");
    }

    #[test]
    fn classify_osascript_not_authorised_maps_to_auth() {
        let s = fake_status(1);
        let err = classify_osascript_output(
            &s,
            b"",
            b"execution error: not authorised to send Apple events to Messages",
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_osascript_not_allowed_assistive_access_maps_to_auth() {
        let s = fake_status(1);
        let err = classify_osascript_output(
            &s,
            b"",
            b"User has not allowed assistive access for osascript",
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_osascript_other_failure_is_transport() {
        let s = fake_status(2);
        let err = classify_osascript_output(&s, b"", b"some other error\n").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_sqlite3_unable_to_open_is_auth() {
        let err = classify_sqlite3_error("Error: unable to open database file");
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_sqlite3_authorization_denied_is_auth() {
        let err = classify_sqlite3_error("Error: authorization denied");
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_sqlite3_other_is_transport() {
        let err = classify_sqlite3_error("Error: no such table: message");
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn parse_sqlite_rows_empty_returns_empty() {
        assert!(parse_sqlite_rows("").is_empty());
    }

    fn build_row(cols: &[&str]) -> String {
        let mut s = cols.join(SQLITE_SEPARATOR);
        s.push_str(SQLITE_NEWLINE);
        s
    }

    #[test]
    fn parse_sqlite_rows_single_row() {
        let row = build_row(&["1", "guid-1", "hello", "0", "0", "3", "+15551234", ""]);
        let rows = parse_sqlite_rows(&row);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.rowid, 1);
        assert_eq!(r.guid, "guid-1");
        assert_eq!(r.text.as_deref(), Some("hello"));
        assert_eq!(r.date, 0);
        assert!(!r.is_from_me);
        assert_eq!(r.handle.as_deref(), Some("+15551234"));
        assert!(r.chat_id.is_none());
    }

    #[test]
    fn parse_sqlite_rows_multi_row_keeps_order() {
        let row = format!(
            "{}{}",
            build_row(&["1", "g1", "hi", "0", "0", "1", "+1", ""]),
            build_row(&["2", "g2", "there", "0", "0", "1", "+1", "chat-X"]),
        );
        let rows = parse_sqlite_rows(&row);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].rowid, 1);
        assert_eq!(rows[1].rowid, 2);
        assert_eq!(rows[1].chat_id.as_deref(), Some("chat-X"));
    }

    #[test]
    fn parse_sqlite_rows_null_text_becomes_none() {
        let row = build_row(&["5", "g5", "", "0", "0", "1", "+1", ""]);
        let rows = parse_sqlite_rows(&row);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].text.is_none());
    }

    #[test]
    fn parse_sqlite_rows_drops_malformed() {
        let row = format!(
            "bad-row-no-columns{nl}{good}",
            nl = SQLITE_NEWLINE,
            good = build_row(&["2", "g2", "t", "0", "0", "1", "+1", ""]),
        );
        let rows = parse_sqlite_rows(&row);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rowid, 2);
    }

    #[test]
    fn parse_sqlite_rows_drops_bad_rowid() {
        let row = build_row(&["NaN", "g", "t", "0", "0", "1", "+1", ""]);
        let rows = parse_sqlite_rows(&row);
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_sqlite_rows_is_from_me_one() {
        let row = build_row(&["7", "g7", "me", "0", "1", "1", "+1", ""]);
        let rows = parse_sqlite_rows(&row);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_from_me);
    }

    #[test]
    fn parse_sqlite_rows_bad_date_defaults_to_zero() {
        let row = build_row(&["8", "g8", "t", "NaN", "0", "1", "+1", ""]);
        let rows = parse_sqlite_rows(&row);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].date, 0);
    }

    #[test]
    fn from_config_copies_relevant_fields() {
        let cfg = cfg_with("/x/osascript", "/y/sqlite3", "/z/db");
        let b = OsaScriptBridge::from_config(&cfg);
        assert_eq!(b.osascript_bin, "/x/osascript");
        assert_eq!(b.sqlite3_bin, "/y/sqlite3");
        assert_eq!(b.chat_db_path.to_string_lossy(), "/z/db");
    }

    #[test]
    fn bridge_clone_and_debug() {
        let b = OsaScriptBridge::from_config(&IMessageConfig::default());
        let c = b.clone();
        let _ = format!("{c:?}");
    }

    #[tokio::test]
    async fn real_bridge_spawn_failure_is_transport_for_osascript() {
        // Intentionally use a binary that cannot exist so we exercise the
        // spawn-failure path even on Linux runners. On macOS this proves
        // the path-not-found case maps to Transport (and is not silently
        // mis-categorised as Auth).
        let mut cfg = IMessageConfig::default();
        cfg.osascript_bin = "/does/not/exist/osascript-xyzzy".into();
        let b = OsaScriptBridge::from_config(&cfg);
        let err = b.run_applescript("(foo)").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    fn fake_status(code: i32) -> std::process::ExitStatus {
        use std::process::Command as StdCommand;
        StdCommand::new("/bin/sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .output()
            .expect("spawn /bin/sh")
            .status
    }
}
